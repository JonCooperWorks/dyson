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
use super::types::{LedgerSummary, LedgerSummaryEntry, SecurityFinding};
use crate::workspace::WorkspaceHandle;

const LEDGER_PATH: &str = "kb/security-harness/findings-ledger.json";
const LEDGER_SCHEMA_VERSION: u32 = 1;

/// A stable, phrasing-independent fingerprint for a finding.
///
/// Keyed on the only three things two *independent* hunts of the same code
/// reliably agree on: the canonical vulnerability class, the file basenames it
/// touches, and the **sink location** (the `file:line` of the vulnerable line).
/// SHA-256'd so it is stable across processes, platforms, and Rust versions (a
/// hand-rolled `Hash` is none of those).
///
/// Everything else is excluded on purpose. The free-text `trust_boundary`,
/// `entry_point`, `sink_or_decision`, `root_cause`, and `title` are all model
/// prose: two runs describe the same IDOR as "unauthenticated internet caller
/// -> user data store" and "any network caller -> in-memory user store", or the
/// same sink as "dictionary lookup with no ownership check" and "retrieves the
/// full record with no identity predicate". Hashing that prose made every re-run
/// of unchanged code mint brand-new `DYS-` keys (live QA: 0 recurring across two
/// consecutive identical runs). The structural anchors below survive re-wording;
/// the prose does not, so it stays out of the fingerprint.
pub(super) fn finding_fingerprint(finding: &SecurityFinding) -> String {
    let class =
        canonical_vulnerability_class(&finding.vulnerability_class).unwrap_or("uncategorized");

    // File identity: basenames only (line numbers dropped here so a finding
    // survives edits that merely shift lines). entry_point often carries a
    // path:line too, so fold its basename in.
    let mut paths: BTreeSet<String> = finding
        .affected_paths
        .iter()
        .map(|p| path_basename(p))
        .filter(|s| s.contains('.'))
        .collect();
    let ep_path = path_basename(&finding.entry_point);
    if ep_path.contains('.') {
        paths.insert(ep_path);
    }

    // Sink location: the precise `file:line` of the vulnerable line, the one
    // structural fact two independent hunts agree on (both cite `app.py:28` for
    // the same IDOR even while wording the prose differently). Taken from the
    // sink description, falling back to the entry point. Absent a `file:line`
    // anchor the fingerprint is class+files only — coarser, but never keyed on
    // prose. Deliberately NOT sourced from `affected_paths`, whose line numbers
    // drift between runs without indicating a different bug.
    let sink = sink_location(finding);

    let canonical = format!(
        "class={class}|paths={}|sink={sink}",
        paths.into_iter().collect::<Vec<_>>().join(","),
    );
    let digest = Sha256::digest(canonical.as_bytes());
    format!("fp-{}", hex16(&digest))
}

/// The vulnerable `basename:line` — the stable anchor two hunts of the same code
/// agree on. Reads the first `path.ext:line` reference out of `sink_or_decision`
/// (where the harness prompt has the model lead with the sink location), falling
/// back to `entry_point`. Empty when neither carries one.
fn sink_location(finding: &SecurityFinding) -> String {
    first_file_line(&finding.sink_or_decision)
        .or_else(|| first_file_line(&finding.entry_point))
        .unwrap_or_default()
}

/// Extract the first `basename:line` from a `path.ext:line` reference embedded
/// in free text, e.g. `app.py:47 — subprocess.run(...)` -> `app.py:47`. No regex
/// dependency: scan whitespace/bracket-delimited tokens for one shaped like a
/// file:line. `<int:user_id>` and `127.0.0.1` are rejected (no `.ext` before a
/// numeric `:line`).
fn first_file_line(s: &str) -> Option<String> {
    for raw in s.split(|c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '"')
    }) {
        let tok = raw.trim_matches(|c: char| {
            !(c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '/' | '-'))
        });
        let Some((path, line)) = tok.rsplit_once(':') else {
            continue;
        };
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let base = path.rsplit('/').next().unwrap_or(path);
        if base.len() >= 3 && base.contains('.') {
            return Some(format!("{}:{}", base.to_ascii_lowercase(), line));
        }
    }
    None
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

/// Reconcile the findings a run will report against the durable ledger,
/// returning the per-finding [`LedgerSummary`] (stable key + new/recurring).
///
/// The fingerprint is derived from the *canonical* checkpoint finding
/// (`canonical_findings`, looked up by `finding.id`) rather than from the
/// rendered `report_findings`. This is the load-bearing detail: the rendered
/// findings differ between the two report paths — in the `valid` path they are
/// re-authored by the report model (which freely rephrases `entry_point`,
/// `sink_or_decision`, `trust_boundary`, etc.), while in the
/// `deterministic_fallback` path they are the raw checkpoint findings. Hashing
/// the model's rephrasing made the SAME bug mint a fresh `DYS-` key whenever the
/// report path differed across runs, so re-runs never matched. Folding back to
/// the checkpoint finding removes the report model from the fingerprint entirely,
/// leaving only the hunt phrasing the fingerprint already absorbs structurally.
///
/// The summary entry is still keyed by the *rendered* `finding.id` so the report
/// renderer can join keys onto the findings it actually prints. A rendered
/// finding whose id is absent from the checkpoint (model renumbered/invented)
/// falls back to fingerprinting itself — degraded, but never wrong.
pub(super) fn reconcile_findings_ledger(
    ledger: &mut FindingsLedger,
    canonical_findings: &[SecurityFinding],
    report_findings: &[SecurityFinding],
    run_id: &str,
) -> LedgerSummary {
    let canonical: BTreeMap<&str, &SecurityFinding> = canonical_findings
        .iter()
        .map(|finding| (finding.id.as_str(), finding))
        .collect();
    let mut summary = LedgerSummary::default();
    for finding in report_findings {
        let fingerprint_source = canonical
            .get(finding.id.as_str())
            .copied()
            .unwrap_or(finding);
        let fingerprint = finding_fingerprint(fingerprint_source);
        let outcome = ledger.upsert(&fingerprint, fingerprint_source, run_id);
        if outcome.recurring {
            summary.recurring_findings += 1;
        } else {
            summary.new_findings += 1;
        }
        summary.entries.push(LedgerSummaryEntry {
            finding_id: finding.id.clone(),
            finding_key: outcome.finding_key,
            recurring: outcome.recurring,
            occurrences: outcome.occurrences,
        });
    }
    summary
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

    /// Regression for the live QA failure: two independent hunts of the SAME
    /// IDOR worded every prose field differently but both pinned the sink to
    /// `app.py:28`. The old prose-token fingerprint minted disjoint keys
    /// (0 recurring); keying on the sink location makes them identical.
    #[test]
    fn fingerprint_keys_on_sink_location_not_prose() {
        let run_a = SecurityFinding {
            vulnerability_class: "auth_authorization".into(),
            affected_paths: vec!["vuln-demo/app.py:22".into()],
            entry_point: "app.py:22 — get_user(user_id) Flask route handler; user_id from URL path"
                .into(),
            sink_or_decision:
                "app.py:28 — USERS.get(user_id) dictionary lookup with no ownership check".into(),
            trust_boundary: "unauthenticated internet caller -> user data store (USERS dict)"
                .into(),
            ..Default::default()
        };
        let run_b = SecurityFinding {
            vulnerability_class: "auth_authorization".into(),
            // line drift in affected_paths must not matter (basename only).
            affected_paths: vec!["vuln-demo/app.py:21".into()],
            entry_point: "app.py:22 — def get_user(user_id) receives <int:user_id> from the route"
                .into(),
            sink_or_decision:
                "app.py:28 — USERS.get(user_id) retrieves the full record with no identity predicate"
                    .into(),
            trust_boundary: "any network caller -> in-memory user store".into(),
            ..Default::default()
        };
        assert_eq!(
            finding_fingerprint(&run_a),
            finding_fingerprint(&run_b),
            "same class + file + sink line must fingerprint identically despite reworded prose"
        );

        // A genuinely different sink line (different bug) must NOT collide.
        let other_line = SecurityFinding {
            sink_or_decision: "app.py:47 — subprocess.run(cmd, shell=True) executes attacker input"
                .into(),
            vulnerability_class: "injection_unsafe_execution".into(),
            ..run_a.clone()
        };
        assert_ne!(
            finding_fingerprint(&run_a),
            finding_fingerprint(&other_line)
        );
    }

    #[test]
    fn first_file_line_extracts_sink_anchor() {
        assert_eq!(
            first_file_line("app.py:47 — subprocess.run(cmd, shell=True)").as_deref(),
            Some("app.py:47")
        );
        assert_eq!(
            first_file_line("the call at (src/api/users.rs:28) returns the row").as_deref(),
            Some("users.rs:28")
        );
        // Not file:line anchors: a host:port-ish literal, a `<int:user_id>`
        // converter, and prose with no location.
        assert_eq!(
            first_file_line("ping -c 1 127.0.0.1 then exit").as_deref(),
            None
        );
        assert_eq!(
            first_file_line("<int:user_id> path converter").as_deref(),
            None
        );
        assert_eq!(
            first_file_line("no location in this sentence").as_deref(),
            None
        );
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

    fn finding_with_id(
        id: &str,
        class: &str,
        path: &str,
        entry: &str,
        sink: &str,
    ) -> SecurityFinding {
        SecurityFinding {
            id: id.into(),
            title: format!("title for {id}"),
            severity: "high".into(),
            vulnerability_class: class.into(),
            affected_paths: vec![path.into()],
            entry_point: entry.into(),
            sink_or_decision: sink.into(),
            trust_boundary: "caller-to-service".into(),
            root_cause: "rc".into(),
            ..Default::default()
        }
    }

    fn in_memory_workspace() -> WorkspaceHandle {
        std::sync::Arc::new(tokio::sync::RwLock::new(Box::new(
            crate::workspace::InMemoryWorkspace::new(),
        )
            as Box<dyn crate::workspace::Workspace>))
    }

    /// Regression for the live QA: running the harness twice against unchanged
    /// code minted brand-new `DYS-` keys and reported 0 recurring on the second
    /// run, because run 1 fell back to the `deterministic_fallback` report path
    /// (findings == raw checkpoint findings) while run 2 produced a `valid`,
    /// model-authored report (findings re-phrased by the report model). The two
    /// paths hashed different text, so the fingerprints — and thus the keys —
    /// diverged. Folding the fingerprint back to the canonical checkpoint
    /// finding makes the key path-independent.
    #[tokio::test]
    async fn rerun_across_report_paths_keeps_keys_and_marks_recurring() {
        // The Swarm-mirrored `kb/` tree, shared across both runs.
        let workspace = in_memory_workspace();
        let store = LedgerStore::new(Some(workspace), PathBuf::from("/unused"));

        // The two confirmed bugs in ./vuln-demo. Identical code → identical
        // canonical checkpoint findings on every run (the hunt's structural
        // output, which the fingerprint already absorbs).
        let idor = finding_with_id(
            "finding-001",
            "auth_authorization",
            "vuln-demo/app.py:14",
            "GET /users/<id> handler",
            "ownership authorization decision",
        );
        let cmdi = finding_with_id(
            "finding-002",
            "injection_unsafe_execution",
            "vuln-demo/app.py:27",
            "POST /ping handler",
            "subprocess shell invocation",
        );
        let canonical = vec![idor.clone(), cmdi.clone()];

        // Run 1: the report stage fell back to deterministic_fallback, so the
        // rendered findings ARE the canonical checkpoint findings verbatim.
        let mut ledger = store.load().await;
        let run1 = reconcile_findings_ledger(&mut ledger, &canonical, &canonical, "sec-run-1");
        store.save(&ledger).await.expect("persist run 1");
        assert_eq!(run1.new_findings, 2, "first run: both findings are new");
        assert_eq!(run1.recurring_findings, 0);

        // Run 2: the report stage produced a VALID, model-authored report. The
        // report model re-phrased every free-text + structural field but kept
        // the finding ids and the underlying bugs.
        let model_idor = SecurityFinding {
            title: "Insecure direct object reference on user lookup".into(),
            trust_boundary: "unauthenticated caller to application server".into(),
            entry_point: "Flask route GET /users/<id> (users_show view)".into(),
            sink_or_decision: "user row returned without an owner check".into(),
            root_cause: "the view never verifies the session owns the requested id".into(),
            ..idor.clone()
        };
        let model_cmdi = SecurityFinding {
            title: "OS command injection via ping host parameter".into(),
            trust_boundary: "remote attacker to host shell".into(),
            entry_point: "Flask route POST /ping (ping_host view)".into(),
            sink_or_decision: "os.system invoked with shell=True on attacker input".into(),
            root_cause: "user-controlled host concatenated into a shell command".into(),
            ..cmdi.clone()
        };
        let model_report = vec![model_idor.clone(), model_cmdi];

        // The model's rephrasing leaves the structural fingerprint untouched:
        // same class, same file basename, same (here: absent) sink line. Keying
        // on structure rather than prose is what makes the key stable whichever
        // report path renders it.
        assert_eq!(
            finding_fingerprint(&idor),
            finding_fingerprint(&model_idor),
            "reworded prose must not perturb the structural fingerprint"
        );

        // A fresh run reloads the persisted ledger before reconciling.
        let mut ledger = store.load().await;
        let run2 = reconcile_findings_ledger(&mut ledger, &canonical, &model_report, "sec-run-2");
        store.save(&ledger).await.expect("persist run 2");

        assert_eq!(
            run2.recurring_findings, 2,
            "re-run of identical code: both findings must be recurring"
        );
        assert_eq!(
            run2.new_findings, 0,
            "a clean re-run must not mint any new findings"
        );

        let run1_keys: BTreeMap<&str, &str> = run1
            .entries
            .iter()
            .map(|e| (e.finding_id.as_str(), e.finding_key.as_str()))
            .collect();
        for entry in &run2.entries {
            assert_eq!(
                run1_keys.get(entry.finding_id.as_str()),
                Some(&entry.finding_key.as_str()),
                "finding {} must keep the SAME DYS- key across runs",
                entry.finding_id
            );
            assert!(
                entry.finding_key.starts_with("DYS-"),
                "key should be a DYS- ledger key, got {}",
                entry.finding_key
            );
            assert_eq!(
                entry.occurrences, 2,
                "a recurring finding's occurrence count must bump on re-sighting"
            );
        }
    }
}
