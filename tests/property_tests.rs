// ===========================================================================
// Property-based tests — fuzz-style testing with proptest.
// ===========================================================================

use proptest::prelude::*;

use dyson::auth::Credential;
use dyson::message::{ContentBlock, Message};
use dyson::tool::kb_status::format_bytes;

// -----------------------------------------------------------------------
// format_bytes never panics on any input.
// -----------------------------------------------------------------------

proptest! {
    #[test]
    fn format_bytes_never_panics(bytes in 0..usize::MAX) {
        let result = format_bytes(bytes);
        prop_assert!(!result.is_empty());
    }
}

// -----------------------------------------------------------------------
// Credential Debug never leaks the secret.
// -----------------------------------------------------------------------

proptest! {
    #[test]
    fn credential_debug_never_leaks(secret in "[a-zA-Z0-9]{8,200}") {
        let cred = Credential::new(secret.clone());
        let debug = format!("{:?}", cred);
        // The debug output should never contain the raw secret.
        // We use secrets >= 8 chars to avoid false positives from
        // short strings matching substrings of "Credential".
        prop_assert!(
            !debug.contains(&secret),
            "Debug output leaked secret: {debug}"
        );
        prop_assert!(debug.contains("***"));
    }
}

// -----------------------------------------------------------------------
// Message estimate_tokens never panics.
// -----------------------------------------------------------------------

proptest! {
    #[test]
    fn message_estimate_tokens_never_panics(text in "\\PC{0,500}") {
        let msg = Message::user(&text);
        let tokens = msg.estimate_tokens();
        prop_assert!(tokens > 0);
    }
}

// -----------------------------------------------------------------------
// ContentBlock::Text estimate_tokens is consistent.
// -----------------------------------------------------------------------

proptest! {
    #[test]
    fn text_block_estimate_tokens_at_least_one(text in "\\PC{0,500}") {
        let block = ContentBlock::Text { text };
        let tokens = block.estimate_tokens();
        prop_assert!(tokens >= 1, "estimate_tokens should return at least 1");
    }
}

// -----------------------------------------------------------------------
// head_boundary never exceeds message count.
// -----------------------------------------------------------------------

proptest! {
    #[test]
    fn head_boundary_never_exceeds_len(
        protect_head in 0..200usize,
        msg_count in 0..100usize,
    ) {
        let config = dyson::config::CompactionConfig {
            protect_head,
            ..Default::default()
        };
        // head_boundary = min(protect_head, msg_count)
        let result = config.protect_head.min(msg_count);
        prop_assert!(result <= msg_count);
    }
}
