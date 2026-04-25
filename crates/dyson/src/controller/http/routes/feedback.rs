// ===========================================================================
// /api/conversations/:id/feedback — emoji ratings shared with Telegram.
// ===========================================================================

use hyper::Request;

use crate::feedback::{FeedbackEntry, FeedbackRating};

use super::super::responses::{Resp, bad_request, json_ok, read_json_capped};
use super::super::state::HttpState;
use super::super::wire::{FeedbackBody, MAX_SMALL_BODY};

pub(crate) fn emoji_to_rating(emoji: &str) -> Option<FeedbackRating> {
    // Mirror crate::controller::telegram::feedback so behaviour matches
    // Telegram exactly.  Kept inline (not re-exported from the telegram
    // module) so the http controller doesn't depend on telegram's wiring.
    match emoji {
        "💩" | "😡" | "🤮"          => Some(FeedbackRating::Terrible),
        "👎"                           => Some(FeedbackRating::Bad),
        "😢" | "😐"                    => Some(FeedbackRating::NotGood),
        "👍" | "👏"                    => Some(FeedbackRating::Good),
        "🔥" | "🎉" | "😂"             => Some(FeedbackRating::VeryGood),
        "❤️" | "❤" | "🤯" | "💯" | "⚡" => Some(FeedbackRating::Excellent),
        _ => None,
    }
}

pub(super) async fn get(state: &HttpState, id: &str) -> Resp {
    let entries = match state.feedback.as_ref() {
        Some(fb) => fb.load(id).unwrap_or_default(),
        None => Vec::new(),
    };
    json_ok(&entries)
}

pub(super) async fn post(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
    id: &str,
) -> Resp {
    let body: FeedbackBody = match read_json_capped(req, MAX_SMALL_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let fb = match state.feedback.as_ref() {
        Some(f) => f,
        None => return bad_request("feedback store not configured"),
    };
    match body.emoji.as_deref().filter(|s| !s.is_empty()) {
        Some(emoji) => {
            let rating = match emoji_to_rating(emoji) {
                Some(r) => r,
                None => return bad_request(&format!("unknown emoji: {emoji}")),
            };
            let entry = FeedbackEntry {
                turn_index: body.turn_index,
                rating,
                score: rating.score(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            if let Err(e) = fb.upsert(id, entry) {
                return bad_request(&format!("save failed: {e}"));
            }
            json_ok(&serde_json::json!({ "ok": true, "rating": rating, "emoji": emoji }))
        }
        None => {
            // Empty emoji = remove existing feedback for this turn.
            if let Err(e) = fb.remove(id, body.turn_index) {
                return bad_request(&format!("remove failed: {e}"));
            }
            json_ok(&serde_json::json!({ "ok": true, "removed": true }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_to_rating_matches_telegram() {
        // Every emoji the Telegram controller honours must round-trip
        // here too — chats started in Telegram are read by the web UI.
        let cases: &[(&str, FeedbackRating)] = &[
            ("💩", FeedbackRating::Terrible),
            ("😡", FeedbackRating::Terrible),
            ("🤮", FeedbackRating::Terrible),
            ("👎", FeedbackRating::Bad),
            ("😢", FeedbackRating::NotGood),
            ("😐", FeedbackRating::NotGood),
            ("👍", FeedbackRating::Good),
            ("👏", FeedbackRating::Good),
            ("🔥", FeedbackRating::VeryGood),
            ("🎉", FeedbackRating::VeryGood),
            ("😂", FeedbackRating::VeryGood),
            ("❤️", FeedbackRating::Excellent),
            ("❤", FeedbackRating::Excellent),
            ("🤯", FeedbackRating::Excellent),
            ("💯", FeedbackRating::Excellent),
            ("⚡", FeedbackRating::Excellent),
        ];
        for (e, want) in cases {
            assert_eq!(emoji_to_rating(e), Some(*want), "emoji {e} should map");
        }
        assert_eq!(emoji_to_rating("🦀"), None);
        assert_eq!(emoji_to_rating(""), None);
    }
}
