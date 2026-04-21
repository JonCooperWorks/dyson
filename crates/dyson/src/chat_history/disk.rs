// ===========================================================================
// DiskChatHistory — JSON files on disk, one per chat.
//
// Active conversations use `{chat_id}.json`.  When rotated, the file
// is renamed to `{chat_id}.{timestamp}.json` and a fresh file is
// created on the next save.
//
// ```text
// ~/.dyson/chats/
//   2102424765.json                      ← current conversation
//   2102424765.2026-03-19T14-30-00.json  ← rotated (old) conversation
//   2102424765.2026-03-18T09-15-22.json  ← even older
// ```
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

/// File-based chat store: one JSON file per chat in a directory.
pub struct DiskChatHistory {
    /// Directory where chat JSON files are stored.
    dir: PathBuf,
}

impl DiskChatHistory {
    /// Create a new disk chat history in the given directory.
    ///
    /// Creates the directory if it doesn't exist.
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Create from a connection string (path with ~ expansion).
    pub fn new_from_connection_string(connection_string: &str) -> Result<Self> {
        let path = resolve_tilde(connection_string);
        Self::new(path)
    }

    fn chat_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{chat_id}.json"))
    }

    /// Directory for externalized media files for a given chat.
    fn media_dir(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{chat_id}_media"))
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
        let path = self.chat_path(chat_id);
        let media_dir = self.media_dir(chat_id);

        // Externalize image data to avoid bloating the JSON file.
        let messages = externalize_images(messages, &media_dir)?;

        let file = std::fs::File::create(&path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &messages)?;
        tracing::debug!(chat_id = chat_id, path = %path.display(), "chat history saved");
        Ok(())
    }

    fn load(&self, chat_id: &str) -> Result<Vec<Message>> {
        let path = self.chat_path(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut messages: Vec<Message> = serde_json::from_str(&content)?;
        let media_dir = self.media_dir(chat_id);

        // Restore externalized image data.
        restore_images(&mut messages, &media_dir);

        tracing::debug!(
            chat_id = chat_id,
            messages = messages.len(),
            "chat history loaded"
        );
        Ok(messages)
    }

    fn rotate(&self, chat_id: &str) -> Result<()> {
        let path = self.chat_path(chat_id);
        if !path.exists() {
            return Ok(());
        }

        let timestamp = Self::rotation_timestamp();
        let rotated = self.dir.join(format!("{chat_id}.{timestamp}.json"));
        std::fs::rename(&path, &rotated)?;
        tracing::info!(
            chat_id = chat_id,
            rotated = %rotated.display(),
            "chat history rotated"
        );
        Ok(())
    }

    /// Scan the chat directory for current (non-archived) chat files,
    /// returned newest-first by file modification time.  Archived files
    /// embed a timestamp in their stem (`{id}.{ts}.json`) so we filter
    /// them out via the `.` test.
    fn list(&self) -> Result<Vec<String>> {
        let dir_iter = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()), // dir missing = no chats yet
        };
        let mut rows: Vec<(String, std::time::SystemTime)> = Vec::new();
        for entry in dir_iter.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.contains('.') => s.to_string(),
                _ => continue,
            };
            // mtime fallback: UNIX_EPOCH if the platform won't tell us.
            // Treats unknowable dates as oldest, which is the safer
            // default than newest.
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            rows.push((stem, mtime));
        }
        rows.sort_by(|a, b| b.1.cmp(&a.1));
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
    // Build a map from raw data pointer → hash in a single pass.
    // Uses the data string's pointer as key to avoid rehashing in the
    // replacement pass.  The hash is computed once per unique image.
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

    // Create media directory and write files.
    std::fs::create_dir_all(media_dir)?;
    for (hash, data) in &to_write {
        let file_path = media_dir.join(format!("{hash}.b64"));
        if !file_path.exists() {
            std::fs::write(&file_path, data)?;
        }
    }

    // Clone messages with data replaced by references (hash lookup, no rehash).
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

/// Restore externalized image data from files.
///
/// Scans for Image blocks where `data` starts with `@media/`, reads the
/// referenced file, and restores the full base64 data.
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
///
/// Not cryptographic, just needs to be deterministic and collision-resistant
/// enough for a handful of images per conversation.
fn simple_hash(data: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let h1 = hasher.finish();
    // Hash again with a different seed for 128 bits of collision resistance.
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

        // Current chat should be empty (file renamed).
        assert!(store.load("chat_1").unwrap().is_empty());

        // A rotated file should exist in the directory.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("chat_1."))
            .collect();
        assert_eq!(files.len(), 1);

        // The rotated file should contain the old messages.
        let rotated_content = std::fs::read_to_string(files[0].path()).unwrap();
        let rotated_msgs: Vec<Message> = serde_json::from_str(&rotated_content).unwrap();
        assert_eq!(rotated_msgs.len(), 1);

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
                    data: "aGVsbG8gd29ybGQ=".into(), // "hello world" in base64
                    media_type: "image/jpeg".into(),
                },
            ]),
            Message::assistant(vec![ContentBlock::Text {
                text: "It's a greeting.".into(),
            }]),
        ];

        store.save("img_chat", &messages).unwrap();

        // The JSON file should NOT contain the raw base64 data.
        let json = std::fs::read_to_string(dir.join("img_chat.json")).unwrap();
        assert!(
            !json.contains("aGVsbG8gd29ybGQ="),
            "base64 should be externalized"
        );
        assert!(json.contains("@media/"), "should contain media reference");

        // Media directory should exist with a .b64 file.
        let media_dir = dir.join("img_chat_media");
        assert!(media_dir.exists(), "media dir should exist");
        let media_files: Vec<_> = std::fs::read_dir(&media_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(media_files.len(), 1, "should have one media file");

        // Load should restore the original data.
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

        // Two files: current + one rotated.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("chat_1"))
            .collect();
        assert_eq!(files.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
