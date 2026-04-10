// ===========================================================================
// Feedback — per-turn quality ratings for fine-tuning / RLHF.
//
// Controller-agnostic types for storing human feedback on assistant
// responses.  The Telegram controller maps emoji reactions to these
// types; other controllers could use different input mechanisms.
//
// Rating scale:
//   -3  Terrible
//   -2  Bad
//   -1  NotGood
//    0  Decent   (default — no explicit feedback)
//   +1  Good
//   +2  VeryGood
//   +3  Excellent
// ===========================================================================

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// FeedbackRating
// ---------------------------------------------------------------------------

/// 7-point rating scale for assistant responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackRating {
    Terrible,  // -3
    Bad,       // -2
    NotGood,   // -1
    Decent,    //  0
    Good,      // +1
    VeryGood,  // +2
    Excellent, // +3
}

impl FeedbackRating {
    /// Numeric score for this rating (-3 to +3).
    pub fn score(self) -> i8 {
        match self {
            Self::Terrible => -3,
            Self::Bad => -2,
            Self::NotGood => -1,
            Self::Decent => 0,
            Self::Good => 1,
            Self::VeryGood => 2,
            Self::Excellent => 3,
        }
    }
}

// ---------------------------------------------------------------------------
// FeedbackEntry
// ---------------------------------------------------------------------------

/// A single feedback entry linking a conversation turn to a rating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEntry {
    /// Index of the assistant message in the conversation's `Vec<Message>`.
    pub turn_index: usize,
    /// The computed rating.
    pub rating: FeedbackRating,
    /// Numeric score (-3 to +3), denormalized for convenient export.
    pub score: i8,
    /// Unix timestamp (seconds) when the feedback was recorded.
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// FeedbackStore
// ---------------------------------------------------------------------------

/// Disk-backed per-chat feedback store.
///
/// Stores feedback entries as `{chat_id}_feedback.json` in the same
/// directory as chat history files.
pub struct FeedbackStore {
    dir: PathBuf,
}

impl FeedbackStore {
    /// Create a new feedback store rooted at the given directory.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Path to the feedback file for a given chat.
    fn feedback_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{chat_id}_feedback.json"))
    }

    /// Load all feedback entries for a chat.
    ///
    /// Returns an empty Vec if no feedback file exists.
    pub fn load(&self, chat_id: &str) -> Result<Vec<FeedbackEntry>> {
        let path = self.feedback_path(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let entries: Vec<FeedbackEntry> = serde_json::from_str(&content)?;
        Ok(entries)
    }

    /// Save feedback entries for a chat (replaces the file).
    fn save(&self, chat_id: &str, entries: &[FeedbackEntry]) -> Result<()> {
        let path = self.feedback_path(chat_id);
        std::fs::create_dir_all(&self.dir)?;
        let file = std::fs::File::create(&path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, entries)?;
        tracing::debug!(chat_id, entries = entries.len(), "feedback saved");
        Ok(())
    }

    /// Insert or update feedback for a specific turn.
    ///
    /// If feedback already exists for `turn_index`, it is replaced
    /// (latest reaction wins).
    pub fn upsert(&self, chat_id: &str, entry: FeedbackEntry) -> Result<()> {
        let mut entries = self.load(chat_id)?;
        let turn_index = entry.turn_index;

        if let Some(existing) = entries.iter_mut().find(|e| e.turn_index == turn_index) {
            *existing = entry;
        } else {
            entries.push(entry);
        }

        self.save(chat_id, &entries)
    }

    /// Remove feedback for a specific turn (reaction removed by user).
    pub fn remove(&self, chat_id: &str, turn_index: usize) -> Result<()> {
        let mut entries = self.load(chat_id)?;
        let before = entries.len();
        entries.retain(|e| e.turn_index != turn_index);
        if entries.len() != before {
            self.save(chat_id, &entries)?;
        }
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_scores() {
        assert_eq!(FeedbackRating::Terrible.score(), -3);
        assert_eq!(FeedbackRating::Bad.score(), -2);
        assert_eq!(FeedbackRating::NotGood.score(), -1);
        assert_eq!(FeedbackRating::Decent.score(), 0);
        assert_eq!(FeedbackRating::Good.score(), 1);
        assert_eq!(FeedbackRating::VeryGood.score(), 2);
        assert_eq!(FeedbackRating::Excellent.score(), 3);
    }

    fn temp_store(name: &str) -> (PathBuf, FeedbackStore) {
        let dir = std::env::temp_dir()
            .join(format!("dyson_feedback_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FeedbackStore::new(dir.clone());
        (dir, store)
    }

    fn make_entry(turn_index: usize, rating: FeedbackRating) -> FeedbackEntry {
        FeedbackEntry {
            turn_index,
            rating,
            score: rating.score(),
            timestamp: 1712750400,
        }
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let (dir, store) = temp_store("load_none");
        let entries = store.load("chat_1").unwrap();
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_and_load() {
        let (dir, store) = temp_store("upsert_load");

        store.upsert("chat_1", make_entry(3, FeedbackRating::Good)).unwrap();
        store.upsert("chat_1", make_entry(7, FeedbackRating::Excellent)).unwrap();

        let entries = store.load("chat_1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].turn_index, 3);
        assert_eq!(entries[0].rating, FeedbackRating::Good);
        assert_eq!(entries[0].score, 1);
        assert_eq!(entries[1].turn_index, 7);
        assert_eq!(entries[1].rating, FeedbackRating::Excellent);
        assert_eq!(entries[1].score, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_overwrites_existing() {
        let (dir, store) = temp_store("upsert_overwrite");

        store.upsert("chat_1", make_entry(3, FeedbackRating::Good)).unwrap();
        store.upsert("chat_1", make_entry(3, FeedbackRating::Excellent)).unwrap();

        let entries = store.load("chat_1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rating, FeedbackRating::Excellent);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_feedback() {
        let (dir, store) = temp_store("remove");

        store.upsert("chat_1", make_entry(3, FeedbackRating::Good)).unwrap();
        store.upsert("chat_1", make_entry(7, FeedbackRating::Bad)).unwrap();

        store.remove("chat_1", 3).unwrap();

        let entries = store.load("chat_1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].turn_index, 7);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let (dir, store) = temp_store("remove_noop");
        store.remove("chat_1", 99).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn feedback_json_roundtrip() {
        let entry = make_entry(5, FeedbackRating::VeryGood);
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: FeedbackEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.turn_index, 5);
        assert_eq!(parsed.rating, FeedbackRating::VeryGood);
        assert_eq!(parsed.score, 2);
    }
}
