//! Message addressing, reply context, and operator authorization.
use super::{is_public_command, types};

/// Returns true if the message text mentions the given bot username.
///
/// Uses a case-insensitive search with a word-boundary check to avoid
/// false positives from longer usernames that happen to contain the bot's
/// name as a substring.
pub(super) fn is_bot_mentioned(msg: &types::Message, bot_username: &str) -> bool {
    if bot_username.is_empty() {
        return false;
    }
    let text = msg
        .text
        .as_deref()
        .or(msg.caption.as_deref())
        .unwrap_or_default();
    let lower = text.to_lowercase();
    let target = format!("@{bot_username}");
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(&target) {
        let after = search_from + rel + target.len();
        // Valid Telegram username chars: [a-zA-Z0-9_]
        let at_boundary = after >= lower.len()
            || !lower.as_bytes()[after].is_ascii_alphanumeric() && lower.as_bytes()[after] != b'_';
        if at_boundary {
            return true;
        }
        search_from += rel + 1;
    }
    false
}

/// If the message is a reply, prepend the original message's text and sender
/// so the agent sees both the replied-to content and the new reply.
pub(super) fn prepend_reply_context(msg: &types::Message, text: String) -> String {
    match msg.reply_to_message {
        Some(ref reply) => {
            let original = reply
                .text
                .as_deref()
                .or(reply.caption.as_deref())
                .unwrap_or("[no text]");
            let sender = reply
                .from
                .as_ref()
                .and_then(|u| u.username.as_deref())
                .unwrap_or("unknown");
            format!("[Replying to message from @{sender}: \"{original}\"]\n\n{text}")
        }
        None => text,
    }
}

/// Returns true if the message is a reply to a message sent by the given bot.
pub(super) fn is_reply_to_bot(msg: &types::Message, bot_id: i64) -> bool {
    msg.reply_to_message
        .as_ref()
        .is_some_and(|reply| reply.from.as_ref().is_some_and(|from| from.id == bot_id))
}

/// Returns true if the sender is an operator (their user ID is in the
/// allowed-chat-ids list).  In Telegram, private-chat IDs equal user IDs,
/// so `allowed_chat_ids` doubles as the operator allowlist.
pub fn is_operator(sender_id: Option<i64>, allowed_ids: &[i64]) -> bool {
    sender_id.is_some_and(|id| allowed_ids.contains(&id))
}

pub(super) fn should_reject(is_group: bool, allowed_ids: &[i64], chat_id: i64) -> bool {
    !is_group && !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id)
}

pub(super) fn should_reject_group_command(
    is_group: bool,
    text: &str,
    sender_id: Option<i64>,
    allowed_ids: &[i64],
) -> bool {
    is_group
        && text.starts_with('/')
        && !is_public_command(text)
        && !is_operator(sender_id, allowed_ids)
}

pub(super) fn is_directed_group_msg(
    msg: &types::Message,
    text: &str,
    bot_username: &str,
    bot_id: i64,
) -> bool {
    text.starts_with('/') || is_bot_mentioned(msg, bot_username) || is_reply_to_bot(msg, bot_id)
}
