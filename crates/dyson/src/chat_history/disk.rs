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
use std::path::PathBuf;

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

        let media_dir = self.media_dir(chat_id);
        let messages = externalize_images(messages, &media_dir)?;

        let path = self.transcript_path(chat_id);
        let file = std::fs::File::create(&path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &messages)?;
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
// Image externalization — keep chat JSON small.
// ---------------------------------------------------------------------------

/// Replace inline base64 image data with file references.
///
/// Hashes the data, writes it to `{media_dir}/{hash}.b64`, and replaces
/// the `data` field with `@media/{hash}`.  Skips images that are already
/// externalized.
fn externalize_images(messages: &[Message], media_dir: &PathBuf) -> Result<Vec<Message>> {
    let mut hash_cache: HashMap<*const str, String> = HashMap::new();
    let mut to_write: HashMap<String, &str> = HashMap::new();
    let mut needs_externalization = false;

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Image { data, .. }
                | ContentBlock::Document { data, .. }
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
        return Ok(messages.to_vec());
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
            }
        })
        .collect();

    tracing::debug!(
        images = to_write.len(),
        media_dir = %media_dir.display(),
        "externalized image data"
    );

    Ok(messages)
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

        let ids: std::collections::HashSet<String> =
            store.list().unwrap().into_iter().collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(!ids.contains("c"), "archive-only chats should not list");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
