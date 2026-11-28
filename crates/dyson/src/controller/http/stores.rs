// ===========================================================================
// HTTP controller — in-memory file + artefact stores.
//
// Two FIFO-evicted ring caches that share an `EvictingStore<T>` core,
// each with its own disk layout:
//
//   FileStore       — agent-produced bytes (image_generate, exploit
//                     PoCs).  `{data_dir}/files/<id>.bin` +
//                     `<id>.meta.json`.
//   ArtefactStore   — agent-produced markdown reports.  Per-chat
//                     subdirs at `{data_dir}/{chat_id}/artefacts/`.
//
// The cap is a memory bound — bytes stay reachable on disk and the
// route handlers hydrate on miss.
// ===========================================================================

use std::collections::HashMap;

/// FIFO-evicting in-memory store keyed by short id.  Both `FileStore`
/// and `ArtefactStore` wrap one of these, only differing in the disk
/// layout (`load_from_disk` / `persist_static`).  The cap is purely a
/// memory bound — bytes stay reachable on disk and the handlers
/// hydrate on miss.
pub(crate) struct EvictingStore<T> {
    pub(crate) items: HashMap<String, T>,
    pub(crate) order: std::collections::VecDeque<String>,
    cap: usize,
}

impl<T> EvictingStore<T> {
    fn with_cap(cap: usize) -> Self {
        Self {
            items: HashMap::new(),
            order: std::collections::VecDeque::new(),
            cap,
        }
    }

    pub(crate) fn put(&mut self, id: String, entry: T) {
        while self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.items.remove(&old);
            }
        }
        self.order.push_back(id.clone());
        self.items.insert(id, entry);
    }
}

/// In-memory store for agent-produced files.  Files are bytes + mime
/// type; the original filename is part of the SSE event the UI shows.
pub(crate) struct FileStore {
    inner: EvictingStore<FileEntry>,
}

impl Default for FileStore {
    fn default() -> Self {
        Self {
            inner: EvictingStore::with_cap(Self::MAX_FILES),
        }
    }
}

// `state.files.lock().unwrap().items.get(id)` shows up in route
// handlers and tests — keep the inner map reachable through `Deref`-
// style accessors instead of a wrapper API that'd force every call
// site to change.
impl std::ops::Deref for FileStore {
    type Target = EvictingStore<FileEntry>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for FileStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Clone)]
pub(crate) enum FileBody {
    /// Small file buffered in RAM (`<= 25 MiB`).
    Memory(Vec<u8>),
    /// Large file kept on disk and streamed on demand (never slurped into RAM).
    Disk { path: std::path::PathBuf, len: u64 },
}

impl FileBody {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Memory(b) => b.len() as u64,
            Self::Disk { len, .. } => *len,
        }
    }

    pub(crate) fn in_memory_bytes(&self) -> usize {
        match self {
            Self::Memory(b) => b.len(),
            Self::Disk { .. } => 0,
        }
    }

    pub(crate) fn as_memory(&self) -> Option<&[u8]> {
        match self {
            Self::Memory(b) => Some(b.as_slice()),
            Self::Disk { .. } => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct FileEntry {
    pub(crate) body: FileBody,
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) chat_id: String,
}

impl FileStore {
    const MAX_FILES: usize = 64;
    /// Aggregate byte budget for cached in-memory file bodies.
    pub(crate) const MAX_BYTES: usize = 128 * 1024 * 1024;
    /// Threshold above which `load_from_disk` returns a streaming `FileBody::Disk`
    /// reference rather than slurping the file into RAM.
    const DISK_STREAM_THRESHOLD: u64 = 25 * 1024 * 1024;

    /// Cache an entry under both the count cap and [`Self::MAX_BYTES`],
    /// evicting oldest first.  Shadows `EvictingStore::put` (reached
    /// through `DerefMut`) so every caller gets the byte bound.  Bodies
    /// are persisted to disk before they are cached, so an in-memory entry
    /// larger than the whole budget is simply not cached.  Disk-backed
    /// entries hold only a path + length (`in_memory_bytes() == 0`) and
    /// stay bounded by `MAX_FILES`.
    pub(crate) fn put(&mut self, id: String, entry: FileEntry) {
        let incoming = entry.body.in_memory_bytes();
        if incoming > Self::MAX_BYTES {
            return;
        }
        let mut total: usize = self
            .inner
            .items
            .values()
            .map(|e| e.body.in_memory_bytes())
            .sum();
        while total + incoming > Self::MAX_BYTES {
            let Some(old) = self.inner.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.inner.items.remove(&old) {
                total -= evicted.body.in_memory_bytes();
            }
        }
        self.inner.put(id, entry);
    }

    /// Load a persisted file from disk.  Returns `None` if the entry
    /// is missing or unreadable.  Called by `get_file` when the
    /// in-memory cache has evicted the id.  Files larger than
    /// `DISK_STREAM_THRESHOLD` are returned as `FileBody::Disk` so they
    /// stream without buffering multi-gigabyte bodies in RAM.
    pub(crate) fn load_from_disk(data_dir: &std::path::Path, id: &str) -> Option<FileEntry> {
        let sub = data_dir.join("files");
        let meta_path = sub.join(format!("{id}.meta.json"));
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_txt = std::fs::read_to_string(&meta_path).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
        let disk_len = std::fs::metadata(&bytes_path).ok()?.len();
        let body = if disk_len > Self::DISK_STREAM_THRESHOLD {
            FileBody::Disk {
                path: bytes_path,
                len: disk_len,
            }
        } else {
            FileBody::Memory(std::fs::read(&bytes_path).ok()?)
        };
        Some(FileEntry {
            body,
            mime: meta
                .get("mime")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream")
                .to_string(),
            name: meta
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("file")
                .to_string(),
            chat_id: meta.get("chat_id").and_then(|v| v.as_str())?.to_string(),
        })
    }

    /// Persist an entry to disk so it survives controller restarts and
    /// is visible from every browser profile pointed at this instance.
    ///
    /// For `FileBody::Memory`, writes `{id}.bin` + `{id}.meta.json`.
    /// For `FileBody::Disk`, attempts an O(1) hard-link into `{id}.bin`
    /// (zero extra disk blocks, survives deletion of the original path)
    /// and returns `Some(bytes_path)` on success.  If hard-linking fails
    /// (e.g. cross-device), avoids duplicating multi-gigabyte files and
    /// returns `None` so the caller keeps streaming from the original path.
    pub(crate) fn persist_static(
        data_dir: &std::path::Path,
        id: &str,
        entry: &FileEntry,
    ) -> Option<std::path::PathBuf> {
        let sub = data_dir.join("files");
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, "failed to create files dir");
            return None;
        }
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_path = sub.join(format!("{id}.meta.json"));
        match &entry.body {
            FileBody::Memory(bytes) => {
                if let Err(e) = std::fs::write(&bytes_path, bytes) {
                    tracing::warn!(error = %e, id, "failed to persist file bytes");
                    return None;
                }
            }
            FileBody::Disk { path, .. } => {
                if let Err(e) = std::fs::hard_link(path, &bytes_path) {
                    tracing::debug!(
                        error = %e,
                        id,
                        "could not hard-link large file into store; streaming from source path"
                    );
                    return None;
                }
            }
        }
        let meta = serde_json::json!({
            "mime": entry.mime,
            "name": entry.name,
            "chat_id": entry.chat_id,
        });
        if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
            tracing::warn!(error = %e, id, "failed to persist file metadata");
        }
        Some(bytes_path)
    }
}

/// Highest `f<N>` id ever persisted under `{data_dir}/files/` so the
/// controller's monotonic counter resumes above any pre-existing
/// entry.  Files load lazily on `GET /api/conversations/<chat>/files/<id>`, so this is a
/// max-id scan, not a hydrate — the previous shape pretended to fill
/// `FileStore.items` but never did.
pub(crate) fn max_file_id(data_dir: &std::path::Path) -> u64 {
    let entries = match std::fs::read_dir(data_dir.join("files")) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".meta.json").map(str::to_string))
        .filter_map(|id| id.strip_prefix('f').and_then(|r| r.parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
}

/// In-memory store for agent-produced artefacts (security-review
/// reports, etc.).  Same FIFO ring as `FileStore` but the entry
/// carries chat scope + markdown body so the Artefacts view can list
/// everything without downloading bodies.
pub(crate) struct ArtefactStore {
    inner: EvictingStore<ArtefactEntry>,
}

impl Default for ArtefactStore {
    fn default() -> Self {
        Self {
            inner: EvictingStore::with_cap(Self::MAX_ARTEFACTS),
        }
    }
}

impl std::ops::Deref for ArtefactStore {
    type Target = EvictingStore<ArtefactEntry>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for ArtefactStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub(crate) struct ArtefactEntry {
    /// Chat this artefact belongs to — used to filter
    /// `/api/conversations/<chat>/artefacts`.
    pub(crate) chat_id: String,
    pub(crate) kind: dyson_core::message::ArtefactKind,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) mime_type: String,
    pub(crate) metadata: Option<serde_json::Value>,
    /// The tool call that produced this artefact, when known.  Lets
    /// the UI wire image artefacts back to the tool panel on chat
    /// reload — without this, a refreshed page shows `image_generate`
    /// as a text-only tool panel with the image orphaned in chat.
    pub(crate) tool_use_id: Option<String>,
    /// UNIX seconds at emission.  UI sorts the list by this.
    pub(crate) created_at: u64,
}

impl ArtefactStore {
    pub(crate) const MAX_ARTEFACTS: usize = 32;

    /// Per-chat artefact dir: `{data_dir}/{chat_id}/artefacts/`.
    pub(crate) fn dir_for_chat(data_dir: &std::path::Path, chat_id: &str) -> std::path::PathBuf {
        data_dir.join(chat_id).join("artefacts")
    }

    /// Best-effort write-through to disk.  Two files per artefact:
    /// `<id>.body` (raw content) and `<id>.meta.json` (kind, title,
    /// chat id, mime, created_at, optional metadata blob).
    pub(crate) fn persist_static(data_dir: &std::path::Path, id: &str, entry: &ArtefactEntry) {
        let sub = Self::dir_for_chat(data_dir, &entry.chat_id);
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, "failed to create artefacts dir");
            return;
        }
        let body_path = sub.join(format!("{id}.body"));
        let meta_path = sub.join(format!("{id}.meta.json"));
        if let Err(e) = std::fs::write(&body_path, &entry.content) {
            tracing::warn!(error = %e, id, "failed to persist artefact body");
            return;
        }
        let kind_str = match serde_json::to_value(entry.kind) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, id, "failed to serialise artefact kind — skipping meta");
                return;
            }
        };
        let meta = serde_json::json!({
            "chat_id": entry.chat_id,
            "kind": kind_str,
            "title": entry.title,
            "mime_type": entry.mime_type,
            "created_at": entry.created_at,
            "metadata": entry.metadata,
            "tool_use_id": entry.tool_use_id,
        });
        // Bail rather than write an empty meta file: an empty
        // `<id>.meta.json` makes the artefact unloadable and would
        // mask the real failure on the next list call.
        let serialised = match serde_json::to_string_pretty(&meta) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, id, "failed to serialise artefact metadata — skipping write");
                return;
            }
        };
        if let Err(e) = std::fs::write(&meta_path, serialised) {
            tracing::warn!(error = %e, id, "failed to persist artefact metadata");
        }
    }

    /// Load a persisted artefact (meta + body) from disk.  Walks each
    /// per-chat `artefacts/` subdir looking for the id — callers don't
    /// need to know the owning chat up front.  Returns `None` if no
    /// chat subdir carries the id.
    pub(crate) fn load_from_disk(data_dir: &std::path::Path, id: &str) -> Option<ArtefactEntry> {
        for entry in std::fs::read_dir(data_dir).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let sub = path.join("artefacts");
            let meta_path = sub.join(format!("{id}.meta.json"));
            if !meta_path.exists() {
                continue;
            }
            return load_artefact_from_subdir(&sub, id);
        }
        None
    }

    /// Load a persisted artefact from a known chat.  Prefer this over
    /// [`Self::load_from_disk`] whenever the caller already has chat
    /// context: restored/imported histories can contain the same
    /// artefact id under different chat directories, and the tuple is
    /// the real identity.
    pub(crate) fn load_from_disk_for_chat(
        data_dir: &std::path::Path,
        chat_id: &str,
        id: &str,
    ) -> Option<ArtefactEntry> {
        let sub = Self::dir_for_chat(data_dir, chat_id);
        load_artefact_from_subdir(&sub, id)
    }

    /// Walk every `{chat_id}/artefacts/` subdir on startup, return the
    /// largest numeric id seen (so the counter never reuses one), and
    /// warm the cache with the newest `MAX_ARTEFACTS` entries.
    ///
    /// Older entries stay on disk only: the list, get and conversation
    /// endpoints all read disk (the in-memory map is just a cache), so
    /// loading every body ever written would only grow RSS with history.
    pub(crate) fn hydrate_from_disk(&mut self, data_dir: &std::path::Path) -> u64 {
        let mut ids: Vec<(u64, String, std::path::PathBuf)> = Vec::new();
        let dir_iter = match std::fs::read_dir(data_dir) {
            Ok(it) => it,
            Err(_) => return 0,
        };
        for chat_entry in dir_iter.flatten() {
            let chat_path = chat_entry.path();
            if !chat_path.is_dir() {
                continue;
            }
            let sub = chat_path.join("artefacts");
            let art_iter = match std::fs::read_dir(&sub) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for e in art_iter.flatten() {
                let Ok(name) = e.file_name().into_string() else {
                    continue;
                };
                // `strip_suffix`, not `trim_end_matches`: the latter
                // strips repeatedly, turning `a1.meta.json.meta.json`
                // into a second `a1`.
                let Some(id) = name.strip_suffix(".meta.json") else {
                    continue;
                };
                let n = id
                    .strip_prefix('a')
                    .and_then(|r| r.parse::<u64>().ok())
                    .unwrap_or(0);
                ids.push((n, id.to_string(), sub.clone()));
            }
        }
        let max_n = ids.iter().map(|(n, _, _)| *n).max().unwrap_or(0);
        // Sort by numeric id so `order` mirrors creation order, then keep
        // only the newest entries that actually load.
        ids.sort_by_key(|(n, _, _)| *n);
        let mut newest: Vec<(String, ArtefactEntry)> = Vec::new();
        for (_, id, sub) in ids.into_iter().rev() {
            if newest.len() >= Self::MAX_ARTEFACTS {
                break;
            }
            if self.inner.items.contains_key(&id) || newest.iter().any(|(seen, _)| *seen == id) {
                continue;
            }
            if let Some(entry) = load_artefact_from_subdir(&sub, &id) {
                newest.push((id, entry));
            }
        }
        for (id, entry) in newest.into_iter().rev() {
            self.inner.put(id, entry);
        }
        max_n
    }
}

/// Load a single artefact from a known per-chat subdir.  Extracted so
/// hydrate avoids re-walking the whole tree per entry.
fn load_artefact_from_subdir(sub: &std::path::Path, id: &str) -> Option<ArtefactEntry> {
    let body = std::fs::read_to_string(sub.join(format!("{id}.body"))).ok()?;
    let meta_txt = std::fs::read_to_string(sub.join(format!("{id}.meta.json"))).ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
    let kind: dyson_core::message::ArtefactKind = meta
        .get("kind")
        .and_then(|k| serde_json::from_value(k.clone()).ok())
        .unwrap_or(dyson_core::message::ArtefactKind::Other);
    Some(ArtefactEntry {
        chat_id: meta
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        kind,
        title: meta
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Artefact")
            .to_string(),
        content: body,
        mime_type: meta
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or("text/markdown")
            .to_string(),
        metadata: meta.get("metadata").cloned().filter(|v| !v.is_null()),
        tool_use_id: meta
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            body: FileBody::Memory(format!("body-{name}").into_bytes()),
            mime: "text/plain".to_string(),
            name: name.to_string(),
            chat_id: "c1".to_string(),
        }
    }

    fn art_entry(chat: &str, n: u64) -> ArtefactEntry {
        ArtefactEntry {
            chat_id: chat.to_string(),
            kind: dyson_core::message::ArtefactKind::Other,
            title: format!("title-{n}"),
            content: format!("content-{n}"),
            mime_type: "text/markdown".to_string(),
            metadata: None,
            tool_use_id: None,
            created_at: n,
        }
    }

    #[test]
    fn evicting_store_drops_oldest_when_at_cap() {
        let mut s: EvictingStore<FileEntry> = EvictingStore::with_cap(3);
        for i in 0..5 {
            s.put(format!("f{i}"), entry(&format!("f{i}")));
        }
        assert_eq!(s.items.len(), 3, "FIFO must hold the cap");
        assert!(!s.items.contains_key("f0"), "oldest evicted");
        assert!(!s.items.contains_key("f1"), "second-oldest evicted");
        assert!(s.items.contains_key("f2"));
        assert!(s.items.contains_key("f3"));
        assert!(s.items.contains_key("f4"));
        // `order` is the source of truth for FIFO; assert insertion order.
        let order: Vec<&str> = s.order.iter().map(String::as_str).collect();
        assert_eq!(order, vec!["f2", "f3", "f4"]);
    }

    #[test]
    fn file_store_round_trips_through_disk() {
        // Round-trip via persist_static / load_from_disk so a controller
        // restart can rehydrate bytes that the in-memory FIFO has dropped.
        let dir = tempfile::tempdir().unwrap();
        let id = "f42";
        let original = entry("hello.txt");
        FileStore::persist_static(dir.path(), id, &original);
        let loaded = FileStore::load_from_disk(dir.path(), id).expect("loaded");
        assert_eq!(loaded.body.as_memory(), original.body.as_memory());
        assert_eq!(loaded.mime, original.mime);
        assert_eq!(loaded.name, original.name);
    }

    #[test]
    fn file_store_load_returns_none_for_missing_id() {
        let dir = tempfile::tempdir().unwrap();
        assert!(FileStore::load_from_disk(dir.path(), "f1").is_none());
        // Even after creating the files dir, missing meta returns None
        // (not a panic) so the cache-miss path can decide what to do.
        std::fs::create_dir_all(dir.path().join("files")).unwrap();
        assert!(FileStore::load_from_disk(dir.path(), "f1").is_none());
    }

    #[test]
    fn max_file_id_skips_non_matching_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        std::fs::create_dir_all(&files).unwrap();
        // Plant a couple of legitimate ids and some files that should
        // be ignored — we don't want a stray `.tmp` to bump the
        // counter.
        for id in &["f1", "f7", "f23"] {
            std::fs::write(files.join(format!("{id}.meta.json")), b"{}").unwrap();
        }
        std::fs::write(files.join("README.txt"), b"").unwrap();
        std::fs::write(files.join("garbage.meta.json"), b"{}").unwrap();
        std::fs::write(files.join("f99.tmp"), b"").unwrap();
        assert_eq!(max_file_id(dir.path()), 23);
    }

    #[test]
    fn max_file_id_returns_zero_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(max_file_id(dir.path()), 0);
    }

    #[test]
    fn artefact_store_persists_and_loads_with_metadata_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = art_entry("c-0001", 7);
        original.metadata = Some(
            serde_json::json!({"file_url": "/api/conversations/c-0001/files/f1", "bytes": 1024}),
        );
        original.tool_use_id = Some("tool_42".to_string());
        ArtefactStore::persist_static(dir.path(), "a7", &original);
        let loaded = ArtefactStore::load_from_disk(dir.path(), "a7").expect("loaded");
        assert_eq!(loaded.chat_id, "c-0001");
        assert_eq!(loaded.title, "title-7");
        assert_eq!(loaded.content, "content-7");
        assert_eq!(loaded.tool_use_id.as_deref(), Some("tool_42"));
        assert_eq!(
            loaded
                .metadata
                .as_ref()
                .and_then(|m| m.get("bytes"))
                .and_then(|v| v.as_u64()),
            Some(1024),
        );
    }

    #[test]
    fn artefact_load_from_disk_finds_id_under_any_chat_subdir() {
        let dir = tempfile::tempdir().unwrap();
        // Plant the same id under one chat — the loader walks all
        // chat subdirs since callers don't know the owner up front.
        ArtefactStore::persist_static(dir.path(), "a1", &art_entry("c-0042", 1));
        let loaded = ArtefactStore::load_from_disk(dir.path(), "a1").unwrap();
        assert_eq!(loaded.chat_id, "c-0042");
        assert!(ArtefactStore::load_from_disk(dir.path(), "a-missing").is_none());
    }

    #[test]
    fn artefact_load_from_disk_for_chat_disambiguates_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        ArtefactStore::persist_static(dir.path(), "a1", &art_entry("c-tsmc", 1));
        ArtefactStore::persist_static(dir.path(), "a1", &art_entry("c-ntnx", 2));

        let ntnx = ArtefactStore::load_from_disk_for_chat(dir.path(), "c-ntnx", "a1").unwrap();
        let tsmc = ArtefactStore::load_from_disk_for_chat(dir.path(), "c-tsmc", "a1").unwrap();

        assert_eq!(ntnx.chat_id, "c-ntnx");
        assert_eq!(ntnx.title, "title-2");
        assert_eq!(tsmc.chat_id, "c-tsmc");
        assert_eq!(tsmc.title, "title-1");
    }

    #[test]
    fn artefact_hydrate_returns_max_id_and_indexes_recent_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Mix two chats; max id is 9.  Hydrate should find at least
        // the newest entries and report 9 so next_id starts at 10.
        for &(chat, n) in &[
            ("c-0001", 1u64),
            ("c-0001", 3),
            ("c-0002", 9),
            ("c-0002", 5),
        ] {
            ArtefactStore::persist_static(dir.path(), &format!("a{n}"), &art_entry(chat, n));
        }
        let mut store = ArtefactStore::default();
        let max_n = store.hydrate_from_disk(dir.path());
        assert_eq!(
            max_n, 9,
            "hydrate must return the largest id ever persisted"
        );
        // Newest must be present in the in-memory index — older ids
        // may be evicted when the disk count exceeds MAX_ARTEFACTS,
        // but we have only four here so all four should be cached.
        assert!(store.items.contains_key("a9"));
        assert!(store.items.contains_key("a5"));
        assert!(store.items.contains_key("a3"));
        assert!(store.items.contains_key("a1"));
    }

    #[test]
    fn artefact_hydrate_caches_only_the_newest_entries() {
        // MEM-01: every reader (list, get, conversation list) falls back
        // to disk, so cold start only needs the newest `MAX_ARTEFACTS`
        // bodies in memory — loading every report ever written made RSS
        // grow with history.  The returned max id still covers all of
        // disk so the id counter never reuses one.
        let dir = tempfile::tempdir().unwrap();
        let n_entries: u64 = (ArtefactStore::MAX_ARTEFACTS as u64) + 18; // 50
        for n in 1..=n_entries {
            ArtefactStore::persist_static(dir.path(), &format!("a{n}"), &art_entry("c-0001", n));
        }
        let mut store = ArtefactStore::default();
        let max_n = store.hydrate_from_disk(dir.path());
        assert_eq!(max_n, n_entries);
        assert_eq!(store.items.len(), ArtefactStore::MAX_ARTEFACTS);
        assert_eq!(store.order.len(), ArtefactStore::MAX_ARTEFACTS);
        assert!(!store.items.contains_key("a1"), "oldest stays on disk only");
        assert!(store.items.contains_key(&format!("a{n_entries}")));
        // Evicted-from-cache entries are still served from disk.
        assert!(ArtefactStore::load_from_disk(dir.path(), "a1").is_some());
    }

    #[test]
    fn artefact_hydrate_ignores_doubled_meta_suffix() {
        // `trim_end_matches` stripped `.meta.json` repeatedly, so a stray
        // `a1.meta.json.meta.json` re-queued `a1` and duplicated it in
        // `order`.
        let dir = tempfile::tempdir().unwrap();
        ArtefactStore::persist_static(dir.path(), "a1", &art_entry("c-0001", 1));
        let sub = ArtefactStore::dir_for_chat(dir.path(), "c-0001");
        std::fs::write(sub.join("a1.meta.json.meta.json"), "{}").unwrap();
        let mut store = ArtefactStore::default();
        store.hydrate_from_disk(dir.path());
        assert_eq!(store.items.len(), 1);
        assert_eq!(
            store.order.len(),
            1,
            "duplicate id in order: {:?}",
            store.order
        );
    }

    #[test]
    fn artefact_put_after_hydrate_stays_at_cap() {
        let dir = tempfile::tempdir().unwrap();
        let extra: u64 = 5;
        let n_entries: u64 = (ArtefactStore::MAX_ARTEFACTS as u64) + extra; // 37
        for n in 1..=n_entries {
            ArtefactStore::persist_static(dir.path(), &format!("a{n}"), &art_entry("c-0001", n));
        }
        let mut store = ArtefactStore::default();
        let _ = store.hydrate_from_disk(dir.path());
        assert_eq!(store.items.len(), ArtefactStore::MAX_ARTEFACTS);
        store.put("a999".into(), art_entry("c-0001", 999));
        assert_eq!(store.items.len(), ArtefactStore::MAX_ARTEFACTS);
        assert!(!store.items.contains_key(&format!("a{}", extra + 1)));
        assert!(store.items.contains_key("a999"));
    }

    fn file_entry(len: usize) -> FileEntry {
        FileEntry {
            body: FileBody::Memory(vec![0u8; len]),
            mime: "application/octet-stream".into(),
            name: "x.bin".into(),
            chat_id: "c-0001".into(),
        }
    }

    #[test]
    fn file_store_bounds_total_cached_bytes() {
        // MEM-06: 64 entries of arbitrary size is not a memory bound.
        let mut store = FileStore::default();
        let chunk = FileStore::MAX_BYTES / 4 + 1;
        for n in 0..8 {
            store.put(format!("f{n}"), file_entry(chunk));
        }
        let total: usize = store.items.values().map(|e| e.body.in_memory_bytes()).sum();
        assert!(total <= FileStore::MAX_BYTES, "cached {total} bytes");
        assert!(store.items.contains_key("f7"), "newest must stay cached");
        assert_eq!(store.order.len(), store.items.len());
        // An entry larger than the whole budget is not cached at all
        // (it is already on disk) and does not flush the cache.
        store.put("huge".into(), file_entry(FileStore::MAX_BYTES + 1));
        assert!(!store.items.contains_key("huge"));
        assert!(store.items.contains_key("f7"));
    }
}
