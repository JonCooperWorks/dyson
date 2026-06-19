//! Cross-run findings ledger + stable finding fingerprints.
//!
//! A single security run's checkpoint dies with the run. The ledger is the one
//! durable, cross-run record: it maps a *stable fingerprint* of a finding to a
//! persistent record, so a bug re-found on a later run reopens the same entry
//! (with a bumped occurrence count) instead of being reported as brand new.
//! Same persistence path as [`super::checkpoint`]: the workspace `kb/` tree
//! (Swarm-mirrored) with a local `.dyson/` fallback when there is no workspace.
//!
//! The fingerprint is also what the in-run dedupe now clusters on
//! ([`super::report::dedupe_findings`]): it is deliberately blind to the
//! free-text `root_cause`/`title` phrasing, so two hunters describing the same
//! flaw in different words collapse into one group.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::taxonomy::canonical_vulnerability_class;
use super::types::SecurityFinding;
use crate::workspace::WorkspaceHandle;

const LEDGER_PATH: &str = "kb/security-harness/findings-ledger.json";
const LEDGER_SCHEMA_VERSION: u32 = 1;

/// Common connectives that carry no signal for identifying a finding. Kept
/// deliberately small: over-stripping would erase the distinctive identifiers
/// (function/route/symbol names) the signature relies on.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "not", "but",
    "then", "than", "when", "where", "which", "while", "must", "should", "could", "would", "only",
    "also", "does", "done", "via", "per",
];

/// A stable, phrasing-independent fingerprint for a finding.
///
/// Built from the *structural* anchors of the finding — its canonical
/// vulnerability class, the file basenames it touches, the trust boundary, and
/// distinctive identifier tokens from the entry point / sink — and SHA-256'd so
/// it is stable across processes, platforms, and Rust versions (a hand-rolled
/// `Hash` is none of those). The free-text `root_cause`/`title` are excluded on
/// purpose: re-phrasings of the same bug must produce the same fingerprint.
pub(super) fn finding_fingerprint(finding: &SecurityFinding) -> String {
    let class =
        canonical_vulnerability_class(&finding.vulnerability_class).unwrap_or("uncategorized");

    // Location anchor: file basenames (line numbers dropped so the fingerprint
    // survives edits that merely shift lines). entry_point often carries a
    // path:line too, so fold it in.
    let mut paths: BTreeSet<String> = finding
        .affected_paths
        .iter()
        .map(|p| path_basename(p))
        .filter(|s| !s.is_empty())
        .collect();
    let ep_path = path_basename(&finding.entry_point);
    if !ep_path.is_empty() && ep_path.contains('.') {
        paths.insert(ep_path);
    }

    let boundary = normalize_ws(&finding.trust_boundary).to_lowercase();

    // Distinctive identifier tokens from the structural fields — route names,
    // symbol names, sink descriptors. Sorted + deduped so ordering/phrasing of
    // the same identifiers yields the same signature.
    let mut sig: BTreeSet<String> = BTreeSet::new();
    for field in [&finding.entry_point, &finding.sink_or_decision] {
        for tok in signature_tokens(field) {
            sig.insert(tok);
        }
    }

    let canonical = format!(
        "class={class}|paths={}|boundary={boundary}|sig={}",
        paths.into_iter().collect::<Vec<_>>().join(","),
        sig.into_iter().collect::<Vec<_>>().join(","),
    );
    let digest = Sha256::digest(canonical.as_bytes());
    format!("fp-{}", hex16(&digest))
}

/// Stable human-facing key minted for a fingerprint on first sighting, e.g.
/// `DYS-1A2B3C4D`. Derived from the fingerprint so it is reproducible.
fn finding_key_for(fingerprint: &str) -> String {
    let suffix = fingerprint.strip_prefix("fp-").unwrap_or(fingerprint);
    format!(
        "DYS-{}",
        suffix.chars().take(8).collect::<String>().to_uppercase()
    )
}

/// Basename with any `:line[:col]` suffix removed: `src/a/b.rs:42` -> `b.rs`.
fn path_basename(p: &str) -> String {
    let p = p.trim();
    let last = p.rsplit('/').next().unwrap_or(p);
    last.split(':').next().unwrap_or(last).trim().to_string()
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Identifier-ish tokens: lowercased runs of `[a-z0-9_]`, length >= 4, not a
/// stopword, not purely numeric.
fn signature_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| t.len() >= 4)
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

fn hex16(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub(super) struct LedgerRecord {
    pub finding_key: String,
    #[serde(default)]
    pub vulnerability_class: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub first_seen_run: String,
    #[serde(default)]
    pub last_seen_run: String,
    #[serde(default)]
    pub occurrences: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct FindingsLedger {
    pub schema_version: u32,
    #[serde(default)]
    pub records: BTreeMap<String, LedgerRecord>,
}

impl Default for FindingsLedger {
    fn default() -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION,
            records: BTreeMap::new(),
        }
    }
}

/// Outcome of upserting one finding into the ledger.
pub(super) struct UpsertOutcome {
    pub finding_key: String,
    pub recurring: bool,
    pub occurrences: u32,
}

impl FindingsLedger {
    /// Record one finding by fingerprint. A first sighting mints a stable
    /// `finding_key`; a re-sighting reopens the existing record, bumps its
    /// occurrence count, and refreshes the last-seen run + display fields.
    pub(super) fn upsert(
        &mut self,
        fingerprint: &str,
        finding: &SecurityFinding,
        run_id: &str,
    ) -> UpsertOutcome {
        if let Some(record) = self.records.get_mut(fingerprint) {
            record.occurrences = record.occurrences.saturating_add(1);
            record.last_seen_run = run_id.to_string();
            record.title = finding.title.clone();
            record.severity = finding.severity.clone();
            UpsertOutcome {
                finding_key: record.finding_key.clone(),
                recurring: true,
                occurrences: record.occurrences,
            }
        } else {
            let finding_key = finding_key_for(fingerprint);
            let record = LedgerRecord {
                finding_key: finding_key.clone(),
                vulnerability_class: finding.vulnerability_class.clone(),
                title: finding.title.clone(),
                severity: finding.severity.clone(),
                first_seen_run: run_id.to_string(),
                last_seen_run: run_id.to_string(),
                occurrences: 1,
            };
            self.records.insert(fingerprint.to_string(), record);
            UpsertOutcome {
                finding_key,
                recurring: false,
                occurrences: 1,
            }
        }
    }
}

/// Loads/saves the [`FindingsLedger`]. Mirrors `checkpoint::CheckpointStore`:
/// workspace `kb/` tree when present (Swarm-mirrored), local `.dyson/` fallback
/// otherwise.
pub(super) struct LedgerStore {
    workspace: Option<WorkspaceHandle>,
    fallback_path: PathBuf,
}

impl LedgerStore {
    pub(super) fn new(workspace: Option<WorkspaceHandle>, working_dir: PathBuf) -> Self {
        Self {
            workspace,
            fallback_path: working_dir
                .join(".dyson")
                .join("security-harness")
                .join("findings-ledger.json"),
        }
    }

    /// Best-effort load. A missing or corrupt ledger yields an empty one — the
    /// ledger is an enrichment, never a gate on producing a report.
    pub(super) async fn load(&self) -> FindingsLedger {
        let body = if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            match guard.get(LEDGER_PATH) {
                Some(body) => Some(body),
                None => {
                    let disk = guard
                        .programs_dir()
                        .and_then(|programs| programs.parent().map(Path::to_path_buf))
                        .map(|root| root.join(LEDGER_PATH));
                    drop(guard);
                    disk.and_then(|path| std::fs::read_to_string(path).ok())
                }
            }
        } else {
            std::fs::read_to_string(&self.fallback_path).ok()
        };
        body.and_then(|b| serde_json::from_str::<FindingsLedger>(&b).ok())
            .filter(|l| l.schema_version == LEDGER_SCHEMA_VERSION)
            .unwrap_or_default()
    }

    /// Best-effort save. Errors are returned for logging but callers must not
    /// fail the run on a ledger write error.
    pub(super) async fn save(&self, ledger: &FindingsLedger) -> std::result::Result<(), String> {
        let body = serde_json::to_string_pretty(ledger).map_err(|e| e.to_string())?;
        if let Some(workspace) = &self.workspace {
            let mut guard = workspace.write().await;
            guard.set(LEDGER_PATH, &body);
            return guard.save().map_err(|e| e.to_string());
        }
        if let Some(parent) = self.fallback_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.fallback_path, body).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(class: &str, path: &str, entry: &str, root_cause: &str) -> SecurityFinding {
        SecurityFinding {
            vulnerability_class: class.into(),
            affected_paths: vec![path.into()],
            entry_point: entry.into(),
            sink_or_decision: "authorization decision".into(),
            trust_boundary: "caller-to-service".into(),
            root_cause: root_cause.into(),
            ..Default::default()
        }
    }

    #[test]
    fn fingerprint_is_stable_across_root_cause_phrasing() {
        // Same class/path/entry/sink/boundary, different prose → same fingerprint.
        let a = finding(
            "auth_authorization",
            "src/api/users.rs:40",
            "GET /users/:id handler",
            "the handler never checks the caller owns the row",
        );
        let b = finding(
            "auth_authorization",
            "src/api/users.rs:55",
            "GET /users/:id handler",
            "missing owner-scoped authorization permits IDOR",
        );
        assert_eq!(
            finding_fingerprint(&a),
            finding_fingerprint(&b),
            "re-phrasings of the same flaw must share a fingerprint (line drift included)"
        );
    }

    #[test]
    fn fingerprint_differs_by_class_and_by_file() {
        let base = finding(
            "auth_authorization",
            "src/api/users.rs:40",
            "GET /users/:id handler",
            "rc",
        );
        let other_class = finding(
            "ssrf_outbound_network",
            "src/api/users.rs:40",
            "GET /users/:id handler",
            "rc",
        );
        let other_file = finding(
            "auth_authorization",
            "src/api/orders.rs:40",
            "GET /users/:id handler",
            "rc",
        );
        assert_ne!(
            finding_fingerprint(&base),
            finding_fingerprint(&other_class)
        );
        assert_ne!(finding_fingerprint(&base), finding_fingerprint(&other_file));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let f = finding("auth_authorization", "src/x.rs:1", "handler", "rc");
        assert_eq!(finding_fingerprint(&f), finding_fingerprint(&f));
        assert!(finding_fingerprint(&f).starts_with("fp-"));
    }

    #[test]
    fn upsert_mints_then_reopens_the_same_key() {
        let mut ledger = FindingsLedger::default();
        let f = finding("auth_authorization", "src/x.rs:1", "handler", "rc");
        let fp = finding_fingerprint(&f);

        let first = ledger.upsert(&fp, &f, "sec-run-1");
        assert!(!first.recurring, "first sighting is new");
        assert_eq!(first.occurrences, 1);
        assert!(first.finding_key.starts_with("DYS-"));

        let second = ledger.upsert(&fp, &f, "sec-run-2");
        assert!(second.recurring, "second sighting reopens the record");
        assert_eq!(second.occurrences, 2);
        assert_eq!(
            second.finding_key, first.finding_key,
            "the stable key must survive across runs"
        );
        let record = &ledger.records[&fp];
        assert_eq!(record.first_seen_run, "sec-run-1");
        assert_eq!(record.last_seen_run, "sec-run-2");
    }

    #[test]
    fn ledger_round_trips_through_json() {
        let mut ledger = FindingsLedger::default();
        let f = finding("auth_authorization", "src/x.rs:1", "handler", "rc");
        ledger.upsert(&finding_fingerprint(&f), &f, "sec-run-1");
        let json = serde_json::to_string(&ledger).expect("serialize");
        let back: FindingsLedger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ledger, back);
    }
}
