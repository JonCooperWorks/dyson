// ===========================================================================
// ChannelWorkspace — write-restricted wrapper for public agents.
//
// Wraps any Workspace implementation and only allows writes to an
// explicit set of writable keys.  Everything else is silently dropped.
// This is a whitelist model: new files are protected by default.
//
// Used by public agents: each channel gets its own workspace directory
// with identity files symlinked from the operator's main workspace.
// The wrapper ensures only memory-related keys can be written.
//
// Attribution: tracks which user triggered each write operation in an
// append-only audit log (`_audit.jsonl`).  The audit file is protected
// by the whitelist — the LLM can read it but not overwrite it.
//
// Journal expiry: prunes journal files (memory/YYYY-MM-DD.md) older
// than a configurable age to bound storage growth.
// ===========================================================================

use std::collections::HashSet;

use crate::error::Result;
use crate::workspace::Workspace;

/// Default maximum age for journal files in days.
const DEFAULT_MAX_JOURNAL_AGE_DAYS: u32 = 90;

/// Audit log file name.  Protected by the whitelist (not in `writable`
/// or any `writable_prefixes`), so the LLM can read but not overwrite.
const AUDIT_LOG_KEY: &str = "_audit.jsonl";

/// A single write-audit record, serialized as one JSON line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WriteRecord {
    /// ISO-8601 UTC timestamp.
    ts: String,
    /// User who triggered the write (e.g., Telegram username).
    user: String,
    /// Workspace file that was written.
    file: String,
    /// Write mode: "set" or "append".
    mode: String,
}

/// Workspace wrapper that only allows writes to explicitly listed keys.
///
/// Reads delegate straight through.  `set` and `append` only forward to
/// the inner workspace when the key is in `writable` or matches a writable
/// prefix (e.g. `"memory/"` allows all journal writes).
///
/// When attribution is set (via [`set_attribution`]), writes are logged to
/// an append-only audit file (`_audit.jsonl`) on the inner workspace.
pub struct ChannelWorkspace {
    inner: Box<dyn Workspace>,
    writable: HashSet<String>,
    writable_prefixes: Vec<String>,
    /// Current user attribution.  Set per-message by the controller.
    attribution: Option<String>,
    /// Maximum journal age in days.  Journals older than this are pruned
    /// on construction.
    max_journal_age_days: u32,
}

impl ChannelWorkspace {
    pub fn new(inner: Box<dyn Workspace>) -> Self {
        Self {
            inner,
            writable: HashSet::new(),
            writable_prefixes: Vec::new(),
            attribution: None,
            max_journal_age_days: DEFAULT_MAX_JOURNAL_AGE_DAYS,
        }
    }

    /// Allow writes to an exact key (e.g. `"MEMORY.md"`).
    pub fn allow(mut self, key: &str) -> Self {
        self.writable.insert(key.to_string());
        self
    }

    /// Allow writes to any key starting with this prefix (e.g. `"memory/"`).
    pub fn allow_prefix(mut self, prefix: &str) -> Self {
        self.writable_prefixes.push(prefix.to_string());
        self
    }

    /// Set the maximum journal age.  Journals older than this are pruned
    /// when [`expire_journals`] is called.
    pub fn max_journal_age_days(mut self, days: u32) -> Self {
        self.max_journal_age_days = days;
        self
    }

    fn can_write(&self, name: &str) -> bool {
        self.writable.contains(name)
            || self.writable_prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }

    /// Record a write to the audit log on the inner workspace.
    ///
    /// Writes directly to `self.inner` (bypassing the whitelist).
    /// `AUDIT_LOG_KEY` is not in the writable set, so the LLM cannot
    /// overwrite or tamper with it — only this internal method can.
    fn audit(&mut self, file: &str, mode: &str) {
        if let Some(user) = &self.attribution {
            let record = WriteRecord {
                ts: utc_now_iso(),
                user: user.clone(),
                file: file.to_string(),
                mode: mode.to_string(),
            };
            if let Ok(json) = serde_json::to_string(&record) {
                let mut line = json;
                line.push('\n');
                self.inner.append(AUDIT_LOG_KEY, &line);
            }
        }
    }

    /// Remove journal files older than `max_journal_age_days`.
    ///
    /// Journals follow the naming convention `memory/YYYY-MM-DD.md`.
    /// Files that don't match the pattern are left untouched.
    pub fn expire_journals(&mut self) {
        let cutoff = days_ago_date(self.max_journal_age_days);
        let files = self.inner.list_files();
        let expired: Vec<String> = files
            .into_iter()
            .filter(|name| {
                if let Some(date) = parse_journal_date(name) {
                    date < cutoff
                } else {
                    false
                }
            })
            .collect();

        for name in &expired {
            // Overwrite with empty string — save() will persist the deletion.
            self.inner.set(name, "");
            tracing::info!(file = name.as_str(), "expired old journal file");
        }

        if !expired.is_empty() {
            tracing::info!(
                count = expired.len(),
                cutoff = cutoff.as_str(),
                max_age_days = self.max_journal_age_days,
                "journal expiry complete"
            );
        }
    }
}

/// Return the current UTC time as an ISO-8601 string (second precision).
fn utc_now_iso() -> String {
    // Use UNIX_EPOCH to avoid a chrono dependency.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d, h, min, s) = unix_to_datetime(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

/// Convert UNIX timestamp to (year, month, day, hour, minute, second).
fn unix_to_datetime(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let s = (secs % 86400) as u32;
    let h = s / 3600;
    let min = (s % 3600) / 60;
    let sec = s % 60;

    // Civil date from day count (Howard Hinnant algorithm).
    let z = (secs / 86400) as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, h, min, sec)
}

/// Return the date string `YYYY-MM-DD` for N days ago.
fn days_ago_date(days: u32) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d, _, _, _) = unix_to_datetime(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Extract the date from a journal filename like `memory/2026-04-09.md`.
fn parse_journal_date(name: &str) -> Option<String> {
    let stem = name.strip_prefix("memory/")?.strip_suffix(".md")?;
    // Validate YYYY-MM-DD format (10 chars, correct separators).
    if stem.len() == 10 && stem.as_bytes()[4] == b'-' && stem.as_bytes()[7] == b'-' {
        Some(stem.to_string())
    } else {
        None
    }
}

impl Workspace for ChannelWorkspace {
    fn get(&self, name: &str) -> Option<String> {
        self.inner.get(name)
    }

    fn set(&mut self, name: &str, content: &str) {
        if self.can_write(name) {
            self.audit(name, "set");
            self.inner.set(name, content);
        }
    }

    fn append(&mut self, name: &str, content: &str) {
        if self.can_write(name) {
            self.audit(name, "append");
            self.inner.append(name, content);
        }
    }

    fn save(&self) -> Result<()> {
        self.inner.save()
    }

    fn list_files(&self) -> Vec<String> {
        self.inner.list_files()
    }

    fn search(&self, pattern: &str) -> Vec<(String, Vec<String>)> {
        self.inner.search(pattern)
    }

    fn system_prompt(&self) -> String {
        self.inner.system_prompt()
    }

    fn journal(&mut self, entry: &str) {
        // Journals write to memory/YYYY-MM-DD.md — allowed by the
        // "memory/" prefix, so delegate directly.
        self.inner.journal(entry);
    }

    fn char_limit(&self, file: &str) -> Option<usize> {
        self.inner.char_limit(file)
    }

    fn nudge_interval(&self) -> usize {
        self.inner.nudge_interval()
    }

    fn memory_search(&self, query: &str) -> Vec<(String, String)> {
        self.inner.memory_search(query)
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        // Public agents don't load skills from the workspace.
        vec![]
    }

    fn programs_dir(&self) -> Option<std::path::PathBuf> {
        // Public agents don't get a programs directory.
        None
    }

    fn set_attribution(&mut self, user: Option<&str>) {
        self.attribution = user.map(|s| s.to_string());
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    #[test]
    fn unlisted_key_write_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("SOUL.md", "Be evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn unlisted_key_append_is_dropped() {
        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.append("SOUL.md", " And evil.");
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    }

    #[test]
    fn allowed_key_write_succeeds() {
        let inner = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "old");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("MEMORY.md", "new");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "new");
    }

    #[test]
    fn prefix_allows_nested_writes() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow_prefix("memory/");

        ws.set("memory/2026-04-09.md", "journal entry");
        assert_eq!(ws.get("memory/2026-04-09.md").unwrap(), "journal entry");
    }

    #[test]
    fn prefix_does_not_allow_exact_match() {
        let inner = InMemoryWorkspace::new()
            .with_file("MEMORY.md", "original");
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow_prefix("memory/");

        // "memory/" prefix does not match "MEMORY.md"
        ws.set("MEMORY.md", "overwrite");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "original");
    }

    #[test]
    fn new_unknown_file_is_protected_by_default() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("EVIL.md", "bad content");
        assert!(ws.get("EVIL.md").is_none());
    }

    #[test]
    fn skill_dirs_returns_empty() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner));
        assert!(ws.skill_dirs().is_empty());
    }

    #[test]
    fn programs_dir_returns_none() {
        let inner = InMemoryWorkspace::new();
        let ws = ChannelWorkspace::new(Box::new(inner));
        assert!(ws.programs_dir().is_none());
    }

    // -- Attribution tests ---------------------------------------------------

    #[test]
    fn write_with_attribution_creates_audit_log() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set_attribution(Some("alice"));
        ws.set("MEMORY.md", "hello");

        let log = ws.get(AUDIT_LOG_KEY).expect("audit log should exist");
        assert!(log.contains("\"user\":\"alice\""));
        assert!(log.contains("\"file\":\"MEMORY.md\""));
        assert!(log.contains("\"mode\":\"set\""));
    }

    #[test]
    fn write_without_attribution_no_audit_log() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set("MEMORY.md", "hello");

        assert!(ws.get(AUDIT_LOG_KEY).is_none());
    }

    #[test]
    fn audit_log_is_append_only() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set_attribution(Some("alice"));
        ws.set("MEMORY.md", "first");

        ws.set_attribution(Some("bob"));
        ws.append("MEMORY.md", "second");

        let log = ws.get(AUDIT_LOG_KEY).unwrap();
        let lines: Vec<&str> = log.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"user\":\"alice\""));
        assert!(lines[1].contains("\"user\":\"bob\""));
    }

    #[test]
    fn audit_log_not_writable_by_llm() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        // Write an audit entry first.
        ws.set_attribution(Some("alice"));
        ws.set("MEMORY.md", "data");

        // LLM tries to overwrite the audit log.
        ws.set(AUDIT_LOG_KEY, "tampered");
        ws.append(AUDIT_LOG_KEY, "tampered");

        let log = ws.get(AUDIT_LOG_KEY).unwrap();
        assert!(!log.contains("tampered"));
        assert!(log.contains("\"user\":\"alice\""));
    }

    #[test]
    fn clear_attribution_stops_auditing() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set_attribution(Some("alice"));
        ws.set("MEMORY.md", "first");

        ws.set_attribution(None);
        ws.set("MEMORY.md", "second");

        let log = ws.get(AUDIT_LOG_KEY).unwrap();
        let lines: Vec<&str> = log.trim().lines().collect();
        assert_eq!(lines.len(), 1, "only one audit entry (from alice)");
    }

    #[test]
    fn blocked_write_not_audited() {
        let inner = InMemoryWorkspace::new();
        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md");

        ws.set_attribution(Some("alice"));
        ws.set("SOUL.md", "evil");

        // No audit entry because the write was blocked.
        assert!(ws.get(AUDIT_LOG_KEY).is_none());
    }

    // -- Journal expiry tests ------------------------------------------------

    #[test]
    fn expire_journals_removes_old_files() {
        let inner = InMemoryWorkspace::new()
            .with_file("memory/2020-01-01.md", "ancient")
            .with_file("memory/2020-06-15.md", "old")
            .with_file("memory/2099-12-31.md", "future");

        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow_prefix("memory/")
            .max_journal_age_days(30);

        ws.expire_journals();

        // Old journals should be empty (expired).
        assert_eq!(ws.get("memory/2020-01-01.md").unwrap(), "");
        assert_eq!(ws.get("memory/2020-06-15.md").unwrap(), "");
        // Future journal should be untouched.
        assert_eq!(ws.get("memory/2099-12-31.md").unwrap(), "future");
    }

    #[test]
    fn expire_journals_ignores_non_journal_files() {
        let inner = InMemoryWorkspace::new()
            .with_file("memory/notes/rust.md", "rust notes")
            .with_file("MEMORY.md", "main memory");

        let mut ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md")
            .allow_prefix("memory/")
            .max_journal_age_days(1);

        ws.expire_journals();

        assert_eq!(ws.get("memory/notes/rust.md").unwrap(), "rust notes");
        assert_eq!(ws.get("MEMORY.md").unwrap(), "main memory");
    }

    // -- Helper function tests -----------------------------------------------

    #[test]
    fn parse_journal_date_valid() {
        assert_eq!(
            parse_journal_date("memory/2026-04-09.md"),
            Some("2026-04-09".to_string())
        );
    }

    #[test]
    fn parse_journal_date_invalid() {
        assert_eq!(parse_journal_date("memory/notes/rust.md"), None);
        assert_eq!(parse_journal_date("MEMORY.md"), None);
        assert_eq!(parse_journal_date("memory/short.md"), None);
    }

    #[test]
    fn audit_record_round_trips_as_json() {
        let record = WriteRecord {
            ts: "2026-04-09T14:30:00Z".to_string(),
            user: "alice".to_string(),
            file: "MEMORY.md".to_string(),
            mode: "set".to_string(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: WriteRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user, "alice");
        assert_eq!(parsed.file, "MEMORY.md");
    }
}
