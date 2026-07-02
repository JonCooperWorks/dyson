// ===========================================================================
// Shared utility functions.
// ===========================================================================

/// Maximum bytes of tool output before truncation.
///
/// 100KB is generous enough for most tool calls (file listings, test output,
/// grep results) but small enough to leave room in the LLM's context window
/// for the conversation history and system prompt.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Truncate a string to at most `max_bytes`, snapping to a UTF-8 char boundary.
///
/// Returns the longest prefix of `s` that is at most `max_bytes` and ends on
/// a valid char boundary.  Returns `s` unchanged if it's already short enough.
pub fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate output to [`MAX_OUTPUT_BYTES`], appending a notice if truncated.
///
/// Returns a `Cow::Borrowed` when no truncation is needed (the common case),
/// avoiding a heap allocation.  Only allocates when the output exceeds the
/// limit and needs the truncation notice appended.
pub fn truncate_output(output: &str) -> std::borrow::Cow<'_, str> {
    if output.len() <= MAX_OUTPUT_BYTES {
        return std::borrow::Cow::Borrowed(output);
    }

    let truncated = truncate_to_char_boundary(output, MAX_OUTPUT_BYTES);
    let remaining = output.len() - truncated.len();
    std::borrow::Cow::Owned(format!(
        "{truncated}\n\n... (output truncated — {remaining} bytes omitted, \
         total was {} bytes)",
        output.len()
    ))
}

/// Escape a string for safe embedding inside single-quoted shell arguments.
///
/// Replaces every `'` with `'\''` which:
///   1. Ends the current single-quoted string
///   2. Adds a literal `'` via `\'`
///   3. Starts a new single-quoted string
///
/// Example: `it's here` → `it'\''s here`
pub fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Expand a leading `~` or `~/` to `$HOME` and return the result as a
/// `PathBuf`.  Leaves every other input (absolute, relative, `~user/…`)
/// unchanged.  Returns `PathBuf::from(path)` verbatim when `HOME` is unset.
pub fn resolve_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    if path == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return std::path::PathBuf::from(home);
    }
    std::path::PathBuf::from(path)
}

/// Exponential backoff with up-to-half jitter: the shared capped-exponential
/// term (`dyson_common::util::backoff_ms`) plus a random `0..=(exp/2)` jitter.
/// The jitter (and thus `rand`) stays dyson-local — the shared helper is pure
/// so swarm can reuse the deterministic term without pulling in `rand`.
pub fn backoff_with_jitter(base_ms: u64, attempt: usize) -> u64 {
    let exp_ms = dyson_common::util::backoff_ms(base_ms, attempt);
    let jitter_ms = rand::random::<u64>() % (exp_ms / 2 + 1);
    exp_ms.saturating_add(jitter_ms)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        let long_output = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let truncated = truncate_output(&long_output);
        assert!(truncated.len() < long_output.len());
        assert!(truncated.contains("truncated"));
        assert!(truncated.contains("1000 bytes omitted"));
    }

    #[test]
    fn no_truncation_for_short_output() {
        let short = "hello world";
        assert_eq!(&*truncate_output(short), short);
    }

    #[test]
    fn resolve_tilde_expands_bare_and_prefix() {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(resolve_tilde("~"), home);
        assert_eq!(resolve_tilde("~/foo/bar"), home.join("foo/bar"));
    }

    #[test]
    fn resolve_tilde_passes_through_other_inputs() {
        assert_eq!(
            resolve_tilde("/etc/passwd"),
            std::path::PathBuf::from("/etc/passwd")
        );
        assert_eq!(
            resolve_tilde("relative/path"),
            std::path::PathBuf::from("relative/path")
        );
        // ~user/ is not supported — per-user home lookup would need `getpwnam`.
        assert_eq!(
            resolve_tilde("~alice/foo"),
            std::path::PathBuf::from("~alice/foo")
        );
    }
}
