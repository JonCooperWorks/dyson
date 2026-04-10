// ===========================================================================
// Telegram-specific feedback — emoji → rating mapping.
//
// Maps Telegram reaction emojis to the domain-level FeedbackRating.
// This is the only Telegram-specific piece; the types and storage live
// in the top-level `feedback` module.
// ===========================================================================

use crate::feedback::FeedbackRating;

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
}
