// ===========================================================================
// Shared utility functions.
// ===========================================================================

/// Maximum bytes of tool output before truncation.
///
/// 100KB is generous enough for most tool calls (file listings, test output,
/// grep results) but small enough to leave room in the LLM's context window
/// for the conversation history and system prompt.
pub(crate) const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Truncate output to [`MAX_OUTPUT_BYTES`], appending a notice if truncated.
///
/// Returns a `Cow::Borrowed` when no truncation is needed (the common case),
/// avoiding a heap allocation.  Only allocates when the output exceeds the
/// limit and needs the truncation notice appended.
///
/// We truncate on a UTF-8 char boundary to avoid producing invalid strings.
/// The notice tells the LLM how much was cut so it can request specific
/// portions if needed.
pub(crate) fn truncate_output(output: &str) -> std::borrow::Cow<'_, str> {
    if output.len() <= MAX_OUTPUT_BYTES {
        return std::borrow::Cow::Borrowed(output);
    }

    // Find the last valid char boundary at or before MAX_OUTPUT_BYTES.
    let mut end = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(end) && end > 0 {
        end -= 1;
    }

    let truncated = &output[..end];
    let remaining = output.len() - end;
    std::borrow::Cow::Owned(format!(
        "{truncated}\n\n... (output truncated — {remaining} bytes omitted, \
         total was {} bytes)",
        output.len()
    ))
}

/// Convert a Unix timestamp (seconds since epoch) to a (year, month, day) tuple.
///
/// Uses a civil-date algorithm derived from Howard Hinnant's `chrono`-compatible
/// formulas.  No external dependencies — pure arithmetic.
pub(crate) fn unix_to_ymd(secs: u64) -> (i64, u64, u64) {
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d)
}

/// Escape a string for safe embedding inside single-quoted shell arguments.
///
/// Replaces every `'` with `'\''` which:
///   1. Ends the current single-quoted string
///   2. Adds a literal `'` via `\'`
///   3. Starts a new single-quoted string
///
/// Example: `it's here` → `it'\''s here`
pub(crate) fn escape_single_quotes(s: &str) -> String {
    s.replace('\'', "'\\''")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_is_1970_01_01() {
        assert_eq!(unix_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn known_date() {
        // 2025-01-15 00:00:00 UTC = 1736899200
        assert_eq!(unix_to_ymd(1736899200), (2025, 1, 15));
    }

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
}
