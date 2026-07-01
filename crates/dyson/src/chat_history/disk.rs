// ===========================================================================
// DiskChatHistory — per-chat subdirectory on disk.
//
// Each chat owns everything it produced under `{dir}/{chat_id}/`:
//
// ```text
// ~/.dyson/chats/
//   c-0001/
//     transcript.json          ← current conversation
//     archives/
//       2026-03-19T14-30-00.json
//       2026-03-18T09-15-22.json
//     media/                    ← externalised image refs
//     artefacts/                ← ArtefactStore populates this
//     files/                    ← FileStore populates this
//     feedback.json             ← FeedbackStore populates this
// ```
//
// Delete-cascade is then `fs::remove_dir_all({chat_id})`.  Rotation is
// a rename inside `{chat_id}/archives/`.  The whole class of
// "orphan artefact tagged with a reused id" bugs goes away because
// an id's artefacts, files, media, and transcript all share a parent
// directory and die together.
//
// A one-shot migration at `DiskChatHistory::new` moves any legacy
// flat-layout files (`{id}.json`, `{id}.TIMESTAMP.json`, `{id}_feedback.json`,
// `{id}_media/`) into the per-chat shape so existing deployments don't
// need manual cleanup.
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::chat_history::ChatHistory;
use crate::error::Result;
use crate::message::{ContentBlock, Message};
use crate::util::resolve_tilde;

/// Prefix used in the `data` field to indicate externalized image data.
/// This cannot be valid base64, so there's no collision risk.
const MEDIA_REF_PREFIX: &str = "@media/";

// ---------------------------------------------------------------------------
// DiskChatHistory
// ---------------------------------------------------------------------------

/// File-based chat store: one directory per chat.
pub struct DiskChatHistory {
    /// Root directory holding every chat subdir.
    dir: PathBuf,
}

impl DiskChatHistory {
    /// Create a new disk chat history rooted at the given directory.
    ///
    /// Creates the directory if it doesn't exist and runs any pending
    /// chat-dir migrations (no-op on a current-version dir).
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        if let Err(e) = crate::chat_history::migrate::migrate(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "chats migration failed");
        }
        Ok(Self { dir })
    }

    /// Create from a connection string (path with ~ expansion).
    pub fn new_from_connection_string(connection_string: &str) -> Result<Self> {
        let path = resolve_tilde(connection_string);
        Self::new(path)
    }

    /// Per-chat root: `{dir}/{chat_id}`.
    pub(crate) fn chat_root(&self, chat_id: &str) -> PathBuf {
        self.dir.join(chat_id)
    }

    fn transcript_path(&self, chat_id: &str) -> PathBuf {
        self.chat_root(chat_id).join("transcript.json")
    }

    fn title_path(&self, chat_id: &str) -> PathBuf {
        self.chat_root(chat_id).join("title.txt")
    }

    fn archives_dir(&self, chat_id: &str) -> PathBuf {
        self.chat_root(chat_id).join("archives")
    }

    /// Directory for externalised media files for a given chat.
    fn media_dir(&self, chat_id: &str) -> PathBuf {
        self.chat_root(chat_id).join("media")
    }

    /// Generate a timestamp string for rotation filenames.
    fn rotation_timestamp() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (y, m, d) = crate::util::unix_to_ymd(now);

        let day_secs = now % 86400;
        let h = day_secs / 3600;
        let min = (day_secs % 3600) / 60;
        let s = day_secs % 60;

        format!("{y:04}-{m:02}-{d:02}T{h:02}-{min:02}-{s:02}")
    }
}

impl ChatHistory for DiskChatHistory {
    fn save(&self, chat_id: &str, messages: &[Message]) -> Result<()> {
        let root = self.chat_root(chat_id);
        std::fs::create_dir_all(&root)?;

        // `None` means no message carried inline media — serialize the
        // borrowed slice directly instead of cloning the whole history.
        let media_dir = self.media_dir(chat_id);
        let externalized = externalize_images(messages, &media_dir)?;
        let to_write: &[Message] = externalized.as_deref().unwrap_or(messages);

        // Temp-file + rename so a crash mid-write can't destroy the
        // previous good transcript (which the state-sync worker would
        // otherwise happily push half-written to swarm).
        let path = self.transcript_path(chat_id);
        write_atomically(&path, |writer| {
            serde_json::to_writer_pretty(writer, to_write)?;
            Ok(())
        })?;
        tracing::debug!(chat_id = chat_id, path = %path.display(), "chat history saved");
        Ok(())
    }

    fn load(&self, chat_id: &str) -> Result<Vec<Message>> {
        let path = self.transcript_path(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut messages: Vec<Message> = serde_json::from_str(&content)?;
        let media_dir = self.media_dir(chat_id);
        restore_images(&mut messages, &media_dir);

        tracing::debug!(
            chat_id = chat_id,
            messages = messages.len(),
            "chat history loaded"
        );
        Ok(messages)
    }

    fn save_title(&self, chat_id: &str, title: &str) -> Result<()> {
        let root = self.chat_root(chat_id);
        std::fs::create_dir_all(&root)?;
        std::fs::write(self.title_path(chat_id), title)?;
        Ok(())
    }

    fn load_title(&self, chat_id: &str) -> Result<Option<String>> {
        let path = self.title_path(chat_id);
        if !path.exists() {
            return Ok(None);
        }
        let title = std::fs::read_to_string(path)?.trim().to_string();
        Ok((!title.is_empty()).then_some(title))
    }

    fn remove_title(&self, chat_id: &str) -> Result<()> {
        let path = self.title_path(chat_id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    fn rotate(&self, chat_id: &str) -> Result<()> {
        let path = self.transcript_path(chat_id);
        if !path.exists() {
            return Ok(());
        }

        let archives = self.archives_dir(chat_id);
        std::fs::create_dir_all(&archives)?;

        let timestamp = Self::rotation_timestamp();
        let rotated = archives.join(format!("{timestamp}.json"));
        std::fs::rename(&path, &rotated)?;
        tracing::info!(
            chat_id = chat_id,
            rotated = %rotated.display(),
            "chat history rotated"
        );
        Ok(())
    }

    /// Cascade delete: remove the entire chat subdir — transcript,
    /// archives, media, artefacts, files, feedback all go together.
    fn remove(&self, chat_id: &str) -> Result<()> {
        let root = self.chat_root(chat_id);
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
            tracing::info!(chat_id = chat_id, path = %root.display(), "chat removed");
        }
        Ok(())
    }

    /// Enumerate chat ids with a current transcript on disk.  Scans
    /// subdirectories of the root — any that contain `transcript.json`
    /// is a live chat, sorted newest-first by transcript mtime.
    fn list(&self) -> Result<Vec<String>> {
        let dir_iter = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };
        let mut rows: Vec<(String, std::time::SystemTime)> = Vec::new();
        for entry in dir_iter.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let transcript = path.join("transcript.json");
            if !transcript.exists() {
                continue;
            }
            let chat_id = match entry.file_name().to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            let mtime = std::fs::metadata(&transcript)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            rows.push((chat_id, mtime));
        }
        // Break mtime ties by chat id (reverse) so rapid back-to-back
        // saves — c-0000, c-0001, c-0002 created within one filesystem
        // tick — don't land in `read_dir`-iteration order, which is
        // nondeterministic and makes the HTTP sidebar jitter.  For
        // zero-padded `c-NNNN` ids this also matches creation order;
        // for Telegram numeric ids the tiebreak is merely stable.
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }
}

// ---------------------------------------------------------------------------
// Atomic file replacement — crash-safe transcript writes.
// ---------------------------------------------------------------------------

/// Write a file atomically: stream into a temp file in the same directory,
/// fsync, then rename over the target.  A reader (including the state-sync
/// worker) only ever observes either the previous complete file or the new
/// complete file; a crash mid-write leaves the previous file untouched.
fn write_atomically(
    path: &std::path::Path,
    write: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> Result<()>,
) -> Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Unique per process + call so concurrent writers can't stomp each
    // other's temp file.  Leading dot keeps it out of the state-sync
    // allowlist (hidden components are never synced).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{file_name}.tmp.{}.{n}", std::process::id()));

    let result = (|| {
        let file = create_replacement_file(&tmp, path)?;
        let mut writer = std::io::BufWriter::new(file);
        write(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn create_replacement_file(tmp: &Path, target: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mode = std::fs::metadata(target)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(tmp)?;
    // `mode()` still flows through umask; restore the exact target mode
    // before rename so replacing a private transcript cannot widen it.
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_replacement_file(tmp: &Path, _target: &Path) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?)
}

// ---------------------------------------------------------------------------
// Image externalization — keep chat JSON small.
// ---------------------------------------------------------------------------

/// Replace inline base64 image data with file references.
///
/// Hashes the data, writes it to `{media_dir}/{hash}.b64`, and replaces
/// the `data` field with `@media/{hash}`.  Skips images that are already
/// externalized.
///
/// Returns `None` when no message carries inline media — the caller can
/// then serialize the original slice without cloning the whole history
/// (this runs on every persist-hook checkpoint, so the no-op path must
/// stay allocation-free).
fn externalize_images(messages: &[Message], media_dir: &PathBuf) -> Result<Option<Vec<Message>>> {
    let mut hash_cache: HashMap<*const str, String> = HashMap::new();
    let mut to_write: HashMap<String, &str> = HashMap::new();
    let mut needs_externalization = false;

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Image { data, .. } | ContentBlock::Document { data, .. }
                    if !data.starts_with(MEDIA_REF_PREFIX) =>
                {
                    needs_externalization = true;
                    let ptr: *const str = data.as_str();
                    hash_cache.entry(ptr).or_insert_with(|| {
                        let hash = simple_hash(data);
                        to_write.entry(hash.clone()).or_insert(data.as_str());
                        hash
                    });
                }
                _ => {}
            }
        }
    }

    if !needs_externalization {
        return Ok(None);
    }

    std::fs::create_dir_all(media_dir)?;
    for (hash, data) in &to_write {
        let file_path = media_dir.join(format!("{hash}.b64"));
        if !file_path.exists() {
            std::fs::write(&file_path, data)?;
        }
    }

    let messages: Vec<Message> = messages
        .iter()
        .map(|msg| {
            let content: Vec<ContentBlock> = msg
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Image { data, media_type }
                        if !data.starts_with(MEDIA_REF_PREFIX) =>
                    {
                        let ptr: *const str = data.as_str();
                        let hash = &hash_cache[&ptr];
                        ContentBlock::Image {
                            data: format!("{MEDIA_REF_PREFIX}{hash}"),
                            media_type: media_type.clone(),
                        }
                    }
                    ContentBlock::Document {
                        data,
                        extracted_text,
                    } if !data.starts_with(MEDIA_REF_PREFIX) => {
                        let ptr: *const str = data.as_str();
                        let hash = &hash_cache[&ptr];
                        ContentBlock::Document {
                            data: format!("{MEDIA_REF_PREFIX}{hash}"),
                            extracted_text: extracted_text.clone(),
                        }
                    }
                    other => other.clone(),
                })
                .collect();
            Message {
                role: msg.role.clone(),
                content,
                cost: msg.cost.clone(),
                context_summary: msg.context_summary,
            }
        })
        .collect();

    tracing::debug!(
        images = to_write.len(),
        media_dir = %media_dir.display(),
        "externalized image data"
    );

    Ok(Some(messages))
}

fn restore_images(messages: &mut [Message], media_dir: &std::path::Path) {
    for msg in messages.iter_mut() {
        for block in msg.content.iter_mut() {
            let data_ref = match block {
                ContentBlock::Image { data, .. } | ContentBlock::Document { data, .. } => data,
                _ => continue,
            };
            if let Some(hash) = data_ref.strip_prefix(MEDIA_REF_PREFIX) {
                let hash = hash.to_string();
                let file_path = media_dir.join(format!("{hash}.b64"));
                match std::fs::read_to_string(&file_path) {
                    Ok(content) => {
                        *data_ref = content;
                    }
                    Err(e) => {
                        tracing::warn!(
                            hash = hash,
                            error = %e,
                            "failed to restore externalized media — keeping reference"
                        );
                    }
                }
            }
        }
    }
}

/// Simple hash of a string — first 16 hex chars of a basic hash.
fn simple_hash(data: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let h1 = hasher.finish();
    data.len().hash(&mut hasher);
    let h2 = hasher.finish();
    format!("{h1:016x}{h2:016x}")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    fn temp_store(name: &str) -> (PathBuf, DiskChatHistory) {
        let dir =
            std::env::temp_dir().join(format!("dyson_chat_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = DiskChatHistory::new(dir.clone()).unwrap();
        (dir, store)
    }

    #[test]
    fn save_and_load() {
        let (dir, store) = temp_store("save_load");

        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![crate::message::ContentBlock::Text {
                text: "hi there".into(),
            }]),
        ];

        store.save("chat_1", &messages).unwrap();
        let loaded = store.load("chat_1").unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, crate::message::Role::User);
        assert_eq!(loaded[1].role, crate::message::Role::Assistant);
        // Per-chat layout: transcript lives in chat_1/transcript.json
        assert!(dir.join("chat_1").join("transcript.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression: save() used to File::create (truncate) the previous
    // good transcript in place and stream JSON into it — a crash
    // mid-write destroyed the chat, and the state-sync worker could push
    // the half-written file to swarm.  The save must go through a temp
    // file + rename in the same directory so a reader only ever observes
    // either the old complete transcript or the new complete one.
    #[test]
    fn save_replaces_transcript_atomically_not_in_place() {
        use std::os::unix::fs::MetadataExt;
        let (dir, store) = temp_store("atomic_save");

        store.save("c", &[Message::user("v1")]).unwrap();
        let path = dir.join("c").join("transcript.json");
        let ino_v1 = std::fs::metadata(&path).unwrap().ino();

        store.save("c", &[Message::user("v2")]).unwrap();
        let ino_v2 = std::fs::metadata(&path).unwrap().ino();

        assert_ne!(
            ino_v1, ino_v2,
            "save must write a temp file and rename it over the transcript \
             (atomic replace), never truncate the previous transcript in place"
        );

        // No temp-file litter left behind in the chat dir.
        let leftovers: Vec<String> = std::fs::read_dir(dir.join("c"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp litter: {leftovers:?}");

        let loaded = store.load("c").unwrap();
        assert_eq!(loaded.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Fault injection on the atomic-write helper: a writer that fails
    // partway must leave the original file byte-for-byte intact.
    #[test]
    fn failed_write_leaves_original_transcript_intact() {
        let dir = std::env::temp_dir().join(format!(
            "dyson_chat_test_atomic_fault_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.json");
        std::fs::write(&path, b"[\"good old content\"]").unwrap();

        let result = write_atomically(&path, |w| {
            use std::io::Write as _;
            w.write_all(b"[\"partial garba")?;
            Err(crate::error::DysonError::Llm("simulated crash".into()))
        });
        assert!(result.is_err(), "the injected failure must propagate");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"[\"good old content\"]",
            "a failed write must never damage the previous transcript"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Persist-hook checkpoints save on every message push — the common
    // no-media path must not deep-clone the whole history first.
    #[test]
    fn externalize_is_a_no_op_without_inline_media() {
        let media_dir =
            std::env::temp_dir().join(format!("dyson_chat_test_no_media_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&media_dir);
        let messages = vec![
            Message::user("plain text"),
            Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
        ];
        let result = externalize_images(&messages, &media_dir).unwrap();
        assert!(
            result.is_none(),
            "text-only history must not be cloned on save"
        );
        assert!(!media_dir.exists(), "no media dir for text-only history");
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let (dir, store) = temp_store("load_none");
        let loaded = store.load("nonexistent").unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_preserves_old_and_clears_current() {
        let (dir, store) = temp_store("rotate");

        store
            .save("chat_1", &[Message::user("old message")])
            .unwrap();
        assert!(!store.load("chat_1").unwrap().is_empty());

        store.rotate("chat_1").unwrap();

        // Current transcript removed (until next save re-seeds it).
        assert!(!dir.join("chat_1").join("transcript.json").exists());
        assert!(store.load("chat_1").unwrap().is_empty());

        // A single archive preserves the rotated transcript.
        let archives = dir.join("chat_1").join("archives");
        let files: Vec<_> = std::fs::read_dir(&archives)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(files.len(), 1);
        let archived: Vec<Message> =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).unwrap()).unwrap();
        assert_eq!(archived.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_nonexistent_is_ok() {
        let (dir, store) = temp_store("rotate_none");
        store.rotate("nonexistent").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_with_images() {
        let (dir, store) = temp_store("images");

        let messages = vec![
            Message::user_multimodal(vec![
                ContentBlock::Text {
                    text: "What's this?".into(),
                },
                ContentBlock::Image {
                    data: "aGVsbG8gd29ybGQ=".into(),
                    media_type: "image/jpeg".into(),
                },
            ]),
            Message::assistant(vec![ContentBlock::Text {
                text: "It's a greeting.".into(),
            }]),
        ];

        store.save("img_chat", &messages).unwrap();

        // The transcript should NOT contain the raw base64 data.
        let transcript = dir.join("img_chat").join("transcript.json");
        let json = std::fs::read_to_string(&transcript).unwrap();
        assert!(
            !json.contains("aGVsbG8gd29ybGQ="),
            "base64 should be externalized"
        );
        assert!(json.contains("@media/"), "should contain media reference");

        // Media lives under the chat's subdir.
        let media_dir = dir.join("img_chat").join("media");
        assert!(media_dir.exists(), "media dir should exist");
        let media_files: Vec<_> = std::fs::read_dir(&media_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(media_files.len(), 1, "should have one media file");

        let loaded = store.load("img_chat").unwrap();
        assert_eq!(loaded.len(), 2);
        match &loaded[0].content[1] {
            ContentBlock::Image { data, media_type } => {
                assert_eq!(data, "aGVsbG8gd29ybGQ=", "base64 should be restored");
                assert_eq!(media_type, "image/jpeg");
            }
            other => panic!("expected Image, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_save_after_rotate() {
        let (dir, store) = temp_store("save_after_rotate");

        store.save("chat_1", &[Message::user("first")]).unwrap();
        store.rotate("chat_1").unwrap();
        store.save("chat_1", &[Message::user("second")]).unwrap();

        let loaded = store.load("chat_1").unwrap();
        assert_eq!(loaded.len(), 1);
        match &loaded[0].content[0] {
            crate::message::ContentBlock::Text { text } => assert_eq!(text, "second"),
            other => panic!("expected Text, got: {other:?}"),
        }

        // Current transcript + exactly one archive.
        assert!(dir.join("chat_1").join("transcript.json").exists());
        let archives: Vec<_> = std::fs::read_dir(dir.join("chat_1").join("archives"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(archives.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_preserves_private_transcript_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, store) = temp_store("mode");
        store
            .save("chat_private", &[Message::user("first")])
            .unwrap();
        let transcript = dir.join("chat_private").join("transcript.json");
        std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600)).unwrap();

        store
            .save("chat_private", &[Message::user("second")])
            .unwrap();

        let mode = std::fs::metadata(&transcript).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "atomic replacement must preserve 0600");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_round_trips_independently_of_transcript() {
        let (dir, store) = temp_store("title");
        store
            .save("chat_t", &[Message::user("first prompt")])
            .unwrap();
        store.save_title("chat_t", "Project Scope").unwrap();

        assert_eq!(
            store.load_title("chat_t").unwrap().as_deref(),
            Some("Project Scope")
        );

        store.remove_title("chat_t").unwrap();
        assert_eq!(store.load_title("chat_t").unwrap(), None);
        assert_eq!(store.load("chat_t").unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_wipes_the_whole_subdir() {
        let (dir, store) = temp_store("remove_cascade");
        store.save("chat_x", &[Message::user("alive")]).unwrap();
        // Drop an unrelated sub-asset in the chat's dir to prove cascade.
        let extra = dir.join("chat_x").join("artefacts");
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(extra.join("a1.meta.json"), b"{}").unwrap();

        store.remove("chat_x").unwrap();
        assert!(!dir.join("chat_x").exists(), "chat dir must be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_returns_chats_with_current_transcript_only() {
        let (dir, store) = temp_store("list");
        store.save("a", &[Message::user("a")]).unwrap();
        store.save("b", &[Message::user("b")]).unwrap();
        // `c` has only an archive — list() must skip it.
        store.save("c", &[Message::user("c")]).unwrap();
        store.rotate("c").unwrap();

        let ids: std::collections::HashSet<String> = store.list().unwrap().into_iter().collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(!ids.contains("c"), "archive-only chats should not list");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
