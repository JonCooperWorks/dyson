// ===========================================================================
// Shared utility functions.
// ===========================================================================

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
}
