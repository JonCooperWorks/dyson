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
        Self { items: HashMap::new(), order: std::collections::VecDeque::new(), cap }
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
        Self { inner: EvictingStore::with_cap(Self::MAX_FILES) }
    }
}

// `state.files.lock().unwrap().items.get(id)` shows up in route
// handlers and tests — keep the inner map reachable through `Deref`-
// style accessors instead of a wrapper API that'd force every call
// site to change.
impl std::ops::Deref for FileStore {
    type Target = EvictingStore<FileEntry>;
    fn deref(&self) -> &Self::Target { &self.inner }
}
impl std::ops::DerefMut for FileStore {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.inner }
}

pub(crate) struct FileEntry {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: String,
    pub(crate) name: String,
}

impl FileStore {
    const MAX_FILES: usize = 64;

    pub(crate) fn put(&mut self, id: String, entry: FileEntry) {
        self.inner.put(id, entry);
    }

    /// Read a persisted file from disk.  Returns `None` if the entry
    /// is missing or unreadable.  Called by `get_file` when the
    /// in-memory cache has evicted the id.
    pub(crate) fn load_from_disk(data_dir: &std::path::Path, id: &str) -> Option<FileEntry> {
        let sub = data_dir.join("files");
        let meta_path = sub.join(format!("{id}.meta.json"));
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_txt = std::fs::read_to_string(&meta_path).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
        let bytes = std::fs::read(&bytes_path).ok()?;
        Some(FileEntry {
            bytes,
            mime: meta.get("mime").and_then(|v| v.as_str()).unwrap_or("application/octet-stream").to_string(),
            name: meta.get("name").and_then(|v| v.as_str()).unwrap_or("file").to_string(),
        })
    }

    /// Write an entry to disk so it survives controller restarts and
    /// is visible from every browser profile pointed at this instance.
    /// Best-effort: disk errors are logged but don't fail the tool
    /// call (the in-memory cache still serves the request).
    pub(crate) fn persist_static(data_dir: &std::path::Path, id: &str, entry: &FileEntry) {
        let sub = data_dir.join("files");
        if let Err(e) = std::fs::create_dir_all(&sub) {
            tracing::warn!(error = %e, "failed to create files dir");
            return;
        }
        let bytes_path = sub.join(format!("{id}.bin"));
        let meta_path = sub.join(format!("{id}.meta.json"));
        if let Err(e) = std::fs::write(&bytes_path, &entry.bytes) {
            tracing::warn!(error = %e, id, "failed to persist file bytes");
            return;
        }
        let meta = serde_json::json!({
            "mime": entry.mime,
            "name": entry.name,
        });
        if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
            tracing::warn!(error = %e, id, "failed to persist file metadata");
        }
    }
}

/// Highest `f<N>` id ever persisted under `{data_dir}/files/` so the
/// controller's monotonic counter resumes above any pre-existing
/// entry.  Files load lazily on `GET /api/files/<id>`, so this is a
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
        Self { inner: EvictingStore::with_cap(Self::MAX_ARTEFACTS) }
    }
}

impl std::ops::Deref for ArtefactStore {
    type Target = EvictingStore<ArtefactEntry>;
    fn deref(&self) -> &Self::Target { &self.inner }
}
impl std::ops::DerefMut for ArtefactStore {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.inner }
}

pub(crate) struct ArtefactEntry {
    /// Chat this artefact belongs to — used to filter
    /// `/api/conversations/<chat>/artefacts`.
    pub(crate) chat_id: String,
    pub(crate) kind: crate::message::ArtefactKind,
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

    pub(crate) fn put(&mut self, id: String, entry: ArtefactEntry) {
        self.inner.put(id, entry);
    }

    /// Per-chat artefact dir: `{data_dir}/{chat_id}/artefacts/`.
    /// The migration in `chat_history::migrate` fans the legacy shared
    /// `artefacts/` dir out into these subdirs at startup.
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
        let kind_str = serde_json::to_value(entry.kind)
            .unwrap_or_else(|_| serde_json::Value::String("other".to_string()));
        let meta = serde_json::json!({
            "chat_id": entry.chat_id,
            "kind": kind_str,
            "title": entry.title,
            "mime_type": entry.mime_type,
            "created_at": entry.created_at,
            "metadata": entry.metadata,
            "tool_use_id": entry.tool_use_id,
        });
        if let Err(e) = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap_or_default()) {
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

    /// Walk every `{chat_id}/artefacts/` subdir on startup and populate
    /// the in-memory index so the list endpoint returns everything
    /// immediately.  Returns the largest numeric id seen.
    pub(crate) fn hydrate_from_disk(&mut self, data_dir: &std::path::Path) -> u64 {
        let mut ids: Vec<(String, std::path::PathBuf)> = Vec::new();
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
                let name = match e.file_name().into_string() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if !name.ends_with(".meta.json") {
                    continue;
                }
                let id = name.trim_end_matches(".meta.json").to_string();
                ids.push((id, sub.clone()));
            }
        }
        // Sort by numeric id so `order` mirrors creation order.
        ids.sort_by_key(|(s, _)| {
            s.strip_prefix('a')
                .and_then(|r| r.parse::<u64>().ok())
                .unwrap_or(0)
        });
        let mut max_n: u64 = 0;
        for (id, sub) in ids {
            if let Some(rest) = id.strip_prefix('a')
                && let Ok(n) = rest.parse::<u64>()
            {
                max_n = max_n.max(n);
            }
            if let Some(entry) = load_artefact_from_subdir(&sub, &id) {
                self.put(id, entry);
            }
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
    let kind: crate::message::ArtefactKind = meta
        .get("kind")
        .and_then(|k| serde_json::from_value(k.clone()).ok())
        .unwrap_or(crate::message::ArtefactKind::Other);
    Some(ArtefactEntry {
        chat_id: meta.get("chat_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        kind,
        title: meta.get("title").and_then(|v| v.as_str()).unwrap_or("Artefact").to_string(),
        content: body,
        mime_type: meta.get("mime_type").and_then(|v| v.as_str()).unwrap_or("text/markdown").to_string(),
        metadata: meta.get("metadata").cloned().filter(|v| !v.is_null()),
        tool_use_id: meta.get("tool_use_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
        created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            bytes: format!("body-{name}").into_bytes(),
            mime: "text/plain".to_string(),
            name: name.to_string(),
        }
    }

    fn art_entry(chat: &str, n: u64) -> ArtefactEntry {
        ArtefactEntry {
            chat_id: chat.to_string(),
            kind: crate::message::ArtefactKind::Other,
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
        assert_eq!(loaded.bytes, original.bytes);
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
        original.metadata = Some(serde_json::json!({"file_url": "/api/files/f1", "bytes": 1024}));
        original.tool_use_id = Some("tool_42".to_string());
        ArtefactStore::persist_static(dir.path(), "a7", &original);
        let loaded = ArtefactStore::load_from_disk(dir.path(), "a7").expect("loaded");
        assert_eq!(loaded.chat_id, "c-0001");
        assert_eq!(loaded.title, "title-7");
        assert_eq!(loaded.content, "content-7");
        assert_eq!(loaded.tool_use_id.as_deref(), Some("tool_42"));
        assert_eq!(
            loaded.metadata.as_ref().and_then(|m| m.get("bytes")).and_then(|v| v.as_u64()),
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
    fn artefact_hydrate_returns_max_id_and_indexes_recent_entries() {
        let dir = tempfile::tempdir().unwrap();
        // Mix two chats; max id is 9.  Hydrate should find at least
        // the newest entries and report 9 so next_id starts at 10.
        for &(chat, n) in &[("c-0001", 1u64), ("c-0001", 3), ("c-0002", 9), ("c-0002", 5)] {
            ArtefactStore::persist_static(
                dir.path(),
                &format!("a{n}"),
                &art_entry(chat, n),
            );
        }
        let mut store = ArtefactStore::default();
        let max_n = store.hydrate_from_disk(dir.path());
        assert_eq!(max_n, 9, "hydrate must return the largest id ever persisted");
        // Newest must be present in the in-memory index — older ids
        // may be evicted when the disk count exceeds MAX_ARTEFACTS,
        // but we have only four here so all four should be cached.
        assert!(store.items.contains_key("a9"));
        assert!(store.items.contains_key("a5"));
        assert!(store.items.contains_key("a3"));
        assert!(store.items.contains_key("a1"));
    }
}
