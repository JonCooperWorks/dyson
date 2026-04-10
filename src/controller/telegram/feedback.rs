// ===========================================================================
// Feedback — emoji reaction-based training signal for fine-tuning / RLHF.
//
// Captures Telegram emoji reactions on bot messages and maps them to a
// 7-point rating scale.  Feedback is stored per-chat alongside the chat
// history and can be included in ShareGPT exports for training data.
//
// Rating scale:
//   -3  Terrible    💩 😡 🤮
//   -2  Bad         👎
//   -1  NotGood     😢 😐
//    0  Decent      (no reaction — the default)
//   +1  Good        👍 👏
//   +2  VeryGood    🔥 🎉 😂
//   +3  Excellent   ❤️ 🤯 💯 ⚡
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

    /// String label for this rating.
    pub fn label(self) -> &'static str {
        match self {
            Self::Terrible => "terrible",
            Self::Bad => "bad",
            Self::NotGood => "not_good",
            Self::Decent => "decent",
            Self::Good => "good",
            Self::VeryGood => "very_good",
            Self::Excellent => "excellent",
        }
    }
}

// ---------------------------------------------------------------------------
// Emoji → Rating mapping
// ---------------------------------------------------------------------------

/// Map a Telegram reaction emoji to a feedback rating.
///
/// Returns `None` for unrecognized emojis (no feedback recorded).
pub fn emoji_to_rating(emoji: &str) -> Option<FeedbackRating> {
    match emoji {
        // Terrible (-3)
        "💩" | "😡" | "🤮" => Some(FeedbackRating::Terrible),

        // Bad (-2)
        "👎" => Some(FeedbackRating::Bad),

        // Not Good (-1)
        "😢" | "😐" => Some(FeedbackRating::NotGood),

        // Good (+1)
        "👍" | "👏" => Some(FeedbackRating::Good),

        // Very Good (+2)
        "🔥" | "🎉" | "😂" => Some(FeedbackRating::VeryGood),

        // Excellent (+3)
        "❤️" | "❤" | "🤯" | "💯" | "⚡" => Some(FeedbackRating::Excellent),

        _ => None,
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
    /// The raw emoji that produced this rating.
    pub emoji: String,
    /// Unix timestamp (seconds) when the reaction was recorded.
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
    pub fn upsert(
        &self,
        chat_id: &str,
        turn_index: usize,
        rating: FeedbackRating,
        emoji: &str,
    ) -> Result<()> {
        let mut entries = self.load(chat_id)?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let entry = FeedbackEntry {
            turn_index,
            rating,
            emoji: emoji.to_string(),
            timestamp,
        };

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
    fn emoji_mapping_positive() {
        assert_eq!(emoji_to_rating("👍"), Some(FeedbackRating::Good));
        assert_eq!(emoji_to_rating("👏"), Some(FeedbackRating::Good));
        assert_eq!(emoji_to_rating("🔥"), Some(FeedbackRating::VeryGood));
        assert_eq!(emoji_to_rating("🎉"), Some(FeedbackRating::VeryGood));
        assert_eq!(emoji_to_rating("😂"), Some(FeedbackRating::VeryGood));
        assert_eq!(emoji_to_rating("❤️"), Some(FeedbackRating::Excellent));
        assert_eq!(emoji_to_rating("❤"), Some(FeedbackRating::Excellent));
        assert_eq!(emoji_to_rating("🤯"), Some(FeedbackRating::Excellent));
        assert_eq!(emoji_to_rating("💯"), Some(FeedbackRating::Excellent));
        assert_eq!(emoji_to_rating("⚡"), Some(FeedbackRating::Excellent));
    }

    #[test]
    fn emoji_mapping_negative() {
        assert_eq!(emoji_to_rating("👎"), Some(FeedbackRating::Bad));
        assert_eq!(emoji_to_rating("😢"), Some(FeedbackRating::NotGood));
        assert_eq!(emoji_to_rating("😐"), Some(FeedbackRating::NotGood));
        assert_eq!(emoji_to_rating("💩"), Some(FeedbackRating::Terrible));
        assert_eq!(emoji_to_rating("😡"), Some(FeedbackRating::Terrible));
        assert_eq!(emoji_to_rating("🤮"), Some(FeedbackRating::Terrible));
    }

    #[test]
    fn unknown_emoji_returns_none() {
        assert_eq!(emoji_to_rating("🦀"), None);
        assert_eq!(emoji_to_rating("🐍"), None);
        assert_eq!(emoji_to_rating("hello"), None);
    }

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

    #[test]
    fn rating_labels() {
        assert_eq!(FeedbackRating::Terrible.label(), "terrible");
        assert_eq!(FeedbackRating::NotGood.label(), "not_good");
        assert_eq!(FeedbackRating::Decent.label(), "decent");
        assert_eq!(FeedbackRating::VeryGood.label(), "very_good");
    }

    fn temp_store(name: &str) -> (PathBuf, FeedbackStore) {
        let dir = std::env::temp_dir()
            .join(format!("dyson_feedback_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FeedbackStore::new(dir.clone());
        (dir, store)
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

        store
            .upsert("chat_1", 3, FeedbackRating::Good, "👍")
            .unwrap();
        store
            .upsert("chat_1", 7, FeedbackRating::Excellent, "❤️")
            .unwrap();

        let entries = store.load("chat_1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].turn_index, 3);
        assert_eq!(entries[0].rating, FeedbackRating::Good);
        assert_eq!(entries[0].emoji, "👍");
        assert_eq!(entries[1].turn_index, 7);
        assert_eq!(entries[1].rating, FeedbackRating::Excellent);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_overwrites_existing() {
        let (dir, store) = temp_store("upsert_overwrite");

        store
            .upsert("chat_1", 3, FeedbackRating::Good, "👍")
            .unwrap();
        store
            .upsert("chat_1", 3, FeedbackRating::Excellent, "❤️")
            .unwrap();

        let entries = store.load("chat_1").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rating, FeedbackRating::Excellent);
        assert_eq!(entries[0].emoji, "❤️");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_feedback() {
        let (dir, store) = temp_store("remove");

        store
            .upsert("chat_1", 3, FeedbackRating::Good, "👍")
            .unwrap();
        store
            .upsert("chat_1", 7, FeedbackRating::Bad, "👎")
            .unwrap();

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
        let entry = FeedbackEntry {
            turn_index: 5,
            rating: FeedbackRating::VeryGood,
            emoji: "🔥".to_string(),
            timestamp: 1712750400,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: FeedbackEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.turn_index, 5);
        assert_eq!(parsed.rating, FeedbackRating::VeryGood);
        assert_eq!(parsed.emoji, "🔥");
        assert_eq!(parsed.timestamp, 1712750400);
    }
}
