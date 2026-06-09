//! Data types for the security_engineer staged harness.
//!
//! Everything here is plain data: stage enum, task/finding/decision structs,
//! coverage tracking, the durable SecurityCheckpoint shape, the four stage
//! output structs the LLM emits, and the report struct rendered into Markdown.
//!
//! LLM output is best-effort. Every Deserialize field carries
//! `#[serde(default)]` so a model that omits, mis-types, or merely renames a
//! field cannot poison the harness — downstream code already tolerates empty
//! ids/strings (normalize_task_ids backfills, dedupe falls back to title, etc.).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{SECURITY_HARNESS_SCHEMA_VERSION, SECURITY_HARNESS_VERSION};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SecurityHarnessStage {
    Recon,
    Hunt,
    Validate,
    Gapfill,
    Dedupe,
    Trace,
    Feedback,
    Report,
}

impl SecurityHarnessStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recon => "recon",
            Self::Hunt => "hunt",
            Self::Validate => "validate",
            Self::Gapfill => "gapfill",
            Self::Dedupe => "dedupe",
            Self::Trace => "trace",
            Self::Feedback => "feedback",
            Self::Report => "report",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "recon" => Some(Self::Recon),
            "hunt" => Some(Self::Hunt),
            "validate" => Some(Self::Validate),
            "gapfill" => Some(Self::Gapfill),
            "dedupe" => Some(Self::Dedupe),
            "trace" => Some(Self::Trace),
            "feedback" => Some(Self::Feedback),
            "report" => Some(Self::Report),
            _ => None,
        }
    }
}

impl fmt::Display for SecurityHarnessStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SecurityTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub attack_class: String,
    #[serde(default)]
    pub scope_hint: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SecurityFinding {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub vulnerability_class: String,
    #[serde(default)]
    pub trust_boundary: String,
    #[serde(default)]
    pub entry_point: String,
    #[serde(default)]
    pub sink_or_decision: String,
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub reachability: String,
    #[serde(default)]
    pub tenant_or_instance_impact: String,
    #[serde(default)]
    pub severity_rationale: String,
    #[serde(default)]
    pub fix_recommendation: String,
}

/// Per-severity counts the SecurityHarnessPanel renders as its findings row.
/// `info` and `informational` fold into `low` — the panel UI has four
/// buckets, and operators read both labels as the same low-priority shelf.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeverityRollup {
    pub critical: u64,
    pub high: u64,
    pub medium: u64,
    pub low: u64,
}

impl SeverityRollup {
    /// Bucket a slice of findings by severity.  Used by the live
    /// `security_engineer: findings critical=N high=N medium=N low=N`
    /// checkpoint event and by the panel-state snapshot.  Pinning both
    /// callers to one function avoids the two implementations drifting
    /// (the bug class that gave us the 2026-06-08 rehydrate regression).
    pub(crate) fn from_findings(findings: &[SecurityFinding]) -> Self {
        let mut r = Self::default();
        for f in findings {
            match f.severity.to_ascii_lowercase().as_str() {
                "critical" => r.critical += 1,
                "high" => r.high += 1,
                "medium" => r.medium += 1,
                "low" | "info" | "informational" => r.low += 1,
                _ => {}
            }
        }
        r
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDecisionKind {
    Confirmed,
    Rejected,
    // Conservative default: an LLM that omits or mistypes the decision
    // field should NOT be treated as having confirmed or rejected anything
    // — surface it as a request for more evidence.
    #[default]
    NeedsMoreEvidence,
    Downgrade,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ValidationDecision {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default)]
    pub decision: ValidationDecisionKind,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CoverageGap {
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub risk: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct VulnerabilityClassCoverage {
    #[serde(default)]
    pub class_id: String,
    #[serde(default)]
    pub class_name: String,
    #[serde(default)]
    pub considered: bool,
    #[serde(default)]
    pub applicable: bool,
    #[serde(default)]
    pub hunted: bool,
    #[serde(default)]
    pub skipped_reason: String,
    #[serde(default)]
    pub high_risk_follow_up: bool,
    #[serde(default)]
    pub checked_and_cleared: bool,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DedupeGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub primary_finding_id: String,
    #[serde(default)]
    pub finding_ids: Vec<String>,
    #[serde(default)]
    pub affected_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TraceResult {
    #[serde(default)]
    pub finding_id: String,
    #[serde(default)]
    pub reachable: bool,
    #[serde(default)]
    pub severity_effect: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TargetRef {
    #[serde(default)]
    pub repo_path: String,
    #[serde(default)]
    pub git_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelMetadata {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportValidationState {
    pub status: String,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl Default for ReportValidationState {
    fn default() -> Self {
        Self {
            status: "not_started".into(),
            errors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageHistoryEntry {
    pub stage: SecurityHarnessStage,
    pub status: String,
    pub started_at: u64,
    pub finished_at: u64,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityCheckpoint {
    pub schema_version: u32,
    pub harness_version: String,
    pub run_id: String,
    pub target: TargetRef,
    pub scope: String,
    pub current_stage: SecurityHarnessStage,
    #[serde(default)]
    pub architecture_context: String,
    #[serde(default)]
    pub completed_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub pending_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub findings_so_far: Vec<SecurityFinding>,
    #[serde(default)]
    pub validation_decisions_so_far: Vec<ValidationDecision>,
    #[serde(default)]
    pub dedupe_groups_so_far: Vec<DedupeGroup>,
    #[serde(default)]
    pub trace_results_so_far: Vec<TraceResult>,
    #[serde(default)]
    pub gapfill_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub coverage_gaps: Vec<CoverageGap>,
    #[serde(default)]
    pub class_coverage: Vec<VulnerabilityClassCoverage>,
    #[serde(default)]
    pub report_draft: Option<SecurityHarnessReport>,
    #[serde(default)]
    pub report_validation_state: ReportValidationState,
    #[serde(default)]
    pub stage_history: Vec<StageHistoryEntry>,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: ModelMetadata,
    #[serde(default)]
    pub completed: bool,
}

impl SecurityCheckpoint {
    pub fn new(
        run_id: String,
        target: TargetRef,
        scope: String,
        model: ModelMetadata,
        now: u64,
    ) -> Self {
        Self {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            harness_version: SECURITY_HARNESS_VERSION.into(),
            run_id,
            target,
            scope,
            current_stage: SecurityHarnessStage::Recon,
            architecture_context: String::new(),
            completed_tasks: Vec::new(),
            pending_tasks: Vec::new(),
            findings_so_far: Vec::new(),
            validation_decisions_so_far: Vec::new(),
            dedupe_groups_so_far: Vec::new(),
            trace_results_so_far: Vec::new(),
            gapfill_tasks: Vec::new(),
            coverage_gaps: Vec::new(),
            class_coverage: Vec::new(),
            report_draft: None,
            report_validation_state: ReportValidationState::default(),
            stage_history: Vec::new(),
            created_at: now,
            updated_at: now,
            model,
            completed: false,
        }
    }

    pub fn checkpoint_path(&self) -> String {
        super::checkpoint::checkpoint_path(&self.run_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityHarnessReport {
    pub schema_version: u32,
    pub run_id: String,
    pub target: TargetRef,
    pub scope: String,
    #[serde(default)]
    pub findings: Vec<SecurityFinding>,
    #[serde(default)]
    pub rejected_candidates: Vec<ValidationDecision>,
    #[serde(default)]
    pub gaps: Vec<CoverageGap>,
    #[serde(default)]
    pub dedupe_groups: Vec<DedupeGroup>,
    #[serde(default)]
    pub trace_evidence: Vec<TraceResult>,
    #[serde(default)]
    pub stage_history: Vec<StageHistoryEntry>,
    #[serde(default)]
    pub class_coverage: Vec<VulnerabilityClassCoverage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ReconStageOutput {
    pub architecture_context: String,
    pub tasks: Vec<SecurityTask>,
    pub coverage_gaps: Vec<CoverageGap>,
    pub class_coverage: Vec<VulnerabilityClassCoverage>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct HuntStageOutput {
    #[serde(default)]
    pub completed_task_ids: Vec<String>,
    #[serde(default)]
    pub findings: Vec<SecurityFinding>,
    #[serde(default)]
    pub gaps: Vec<CoverageGap>,
    #[serde(default)]
    pub follow_up_tasks: Vec<SecurityTask>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ValidateStageOutput {
    #[serde(default)]
    pub decisions: Vec<ValidationDecision>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct TraceStageOutput {
    #[serde(default)]
    pub traces: Vec<TraceResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_parse_round_trips_as_str_for_every_variant() {
        for stage in [
            SecurityHarnessStage::Recon,
            SecurityHarnessStage::Hunt,
            SecurityHarnessStage::Validate,
            SecurityHarnessStage::Gapfill,
            SecurityHarnessStage::Dedupe,
            SecurityHarnessStage::Trace,
            SecurityHarnessStage::Feedback,
            SecurityHarnessStage::Report,
        ] {
            assert_eq!(
                SecurityHarnessStage::parse(stage.as_str()),
                Some(stage),
                "stage {stage} should round-trip through as_str/parse"
            );
        }
    }

    #[test]
    fn stage_parse_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(
            SecurityHarnessStage::parse("  RECON  "),
            Some(SecurityHarnessStage::Recon),
            "parse should trim whitespace and normalize case"
        );
        assert_eq!(
            SecurityHarnessStage::parse("Hunt"),
            Some(SecurityHarnessStage::Hunt),
            "parse should be case-insensitive"
        );
    }

    #[test]
    fn stage_parse_returns_none_for_unknown_string() {
        assert_eq!(
            SecurityHarnessStage::parse("not_a_stage"),
            None,
            "parse should return None for unknown stage names"
        );
    }

    #[test]
    fn stage_display_matches_as_str() {
        for stage in [
            SecurityHarnessStage::Recon,
            SecurityHarnessStage::Hunt,
            SecurityHarnessStage::Validate,
            SecurityHarnessStage::Gapfill,
            SecurityHarnessStage::Dedupe,
            SecurityHarnessStage::Trace,
            SecurityHarnessStage::Feedback,
            SecurityHarnessStage::Report,
        ] {
            assert_eq!(
                format!("{stage}"),
                stage.as_str(),
                "Display for {stage:?} should match as_str"
            );
        }
    }

    #[test]
    fn stage_canonical_ordering_matches_pipeline_sequence() {
        // SecurityHarnessStage derives Ord; the implicit derive order must
        // match the canonical pipeline order so BTreeMap<Stage, _> iterates
        // in run order.
        let ordered = [
            SecurityHarnessStage::Recon,
            SecurityHarnessStage::Hunt,
            SecurityHarnessStage::Validate,
            SecurityHarnessStage::Gapfill,
            SecurityHarnessStage::Dedupe,
            SecurityHarnessStage::Trace,
            SecurityHarnessStage::Feedback,
            SecurityHarnessStage::Report,
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0] < pair[1],
                "expected {:?} < {:?} in canonical order",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn report_validation_state_default_is_not_started() {
        let state = ReportValidationState::default();
        assert_eq!(
            state.status, "not_started",
            "default status should be not_started"
        );
        assert!(
            state.errors.is_empty(),
            "default errors should be empty, got {:?}",
            state.errors
        );
    }

    #[test]
    fn security_checkpoint_new_sets_default_fields() {
        let cp = SecurityCheckpoint::new(
            "run-x".into(),
            TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            42,
        );
        assert_eq!(
            cp.schema_version, SECURITY_HARNESS_SCHEMA_VERSION,
            "schema_version should match SECURITY_HARNESS_SCHEMA_VERSION"
        );
        assert_eq!(
            cp.harness_version, SECURITY_HARNESS_VERSION,
            "harness_version should match SECURITY_HARNESS_VERSION"
        );
        assert_eq!(
            cp.current_stage,
            SecurityHarnessStage::Recon,
            "new checkpoint should start at Recon stage"
        );
        assert_eq!(
            cp.created_at, cp.updated_at,
            "created_at and updated_at should match on construction"
        );
        assert_eq!(cp.created_at, 42, "created_at should be the supplied now");
        assert!(!cp.completed, "new checkpoint should not be completed");
        assert!(
            cp.completed_tasks.is_empty(),
            "completed_tasks should be empty"
        );
        assert!(cp.pending_tasks.is_empty(), "pending_tasks should be empty");
        assert!(
            cp.findings_so_far.is_empty(),
            "findings_so_far should be empty"
        );
        assert!(
            cp.validation_decisions_so_far.is_empty(),
            "validation_decisions_so_far should be empty"
        );
        assert!(
            cp.dedupe_groups_so_far.is_empty(),
            "dedupe_groups_so_far should be empty"
        );
        assert!(
            cp.trace_results_so_far.is_empty(),
            "trace_results_so_far should be empty"
        );
        assert!(cp.gapfill_tasks.is_empty(), "gapfill_tasks should be empty");
        assert!(cp.coverage_gaps.is_empty(), "coverage_gaps should be empty");
        assert!(
            cp.class_coverage.is_empty(),
            "class_coverage should be empty"
        );
        assert!(cp.stage_history.is_empty(), "stage_history should be empty");
    }

    #[test]
    fn security_checkpoint_path_uses_run_id_under_kb_prefix() {
        let cp = SecurityCheckpoint::new(
            "sec-12345-7".into(),
            TargetRef::default(),
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        );
        assert_eq!(
            cp.checkpoint_path(),
            "kb/security-harness/checkpoints/sec-12345-7.json",
            "checkpoint_path should embed the run_id under the kb prefix"
        );
    }

    #[test]
    fn validation_decision_kind_default_is_needs_more_evidence() {
        assert_eq!(
            ValidationDecisionKind::default(),
            ValidationDecisionKind::NeedsMoreEvidence,
            "default ValidationDecisionKind must be NeedsMoreEvidence so a \
             missing/mistyped field does not silently confirm a finding"
        );
    }

    #[test]
    fn recon_stage_output_deserializes_from_empty_object() {
        let recon: ReconStageOutput = serde_json::from_str("{}").expect("{{}} should parse");
        assert!(recon.architecture_context.is_empty());
        assert!(recon.tasks.is_empty());
        assert!(recon.coverage_gaps.is_empty());
        assert!(recon.class_coverage.is_empty());
    }

    #[test]
    fn hunt_stage_output_deserializes_from_empty_object() {
        let hunt: HuntStageOutput = serde_json::from_str("{}").expect("{{}} should parse");
        assert!(hunt.completed_task_ids.is_empty());
        assert!(hunt.findings.is_empty());
        assert!(hunt.gaps.is_empty());
        assert!(hunt.follow_up_tasks.is_empty());
    }

    #[test]
    fn validate_stage_output_deserializes_from_empty_object() {
        let v: ValidateStageOutput = serde_json::from_str("{}").expect("{{}} should parse");
        assert!(v.decisions.is_empty());
    }

    #[test]
    fn trace_stage_output_deserializes_from_empty_object() {
        let t: TraceStageOutput = serde_json::from_str("{}").expect("{{}} should parse");
        assert!(t.traces.is_empty());
    }

    #[test]
    fn security_checkpoint_round_trips_through_json() {
        let mut cp = SecurityCheckpoint::new(
            "round-trip".into(),
            TargetRef {
                repo_path: "/repo".into(),
                git_ref: Some("deadbeef".into()),
            },
            "scope".into(),
            ModelMetadata {
                provider: "Anthropic".into(),
                model: "claude".into(),
            },
            100,
        );
        cp.architecture_context = "context".into();
        cp.pending_tasks.push(SecurityTask {
            id: "t1".into(),
            attack_class: "auth_authorization".into(),
            scope_hint: "scope".into(),
            status: TaskStatus::Pending,
            rationale: "r".into(),
        });
        cp.findings_so_far.push(SecurityFinding {
            id: "f1".into(),
            title: "title".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "boundary".into(),
            entry_point: "src/lib.rs:1".into(),
            sink_or_decision: "decision".into(),
            root_cause: "cause".into(),
            affected_paths: vec!["src/lib.rs".into()],
            evidence: vec!["evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "impact".into(),
            severity_rationale: "rationale".into(),
            fix_recommendation: "fix".into(),
        });
        cp.stage_history.push(StageHistoryEntry {
            stage: SecurityHarnessStage::Recon,
            status: "completed".into(),
            started_at: 1,
            finished_at: 2,
            summary: "done".into(),
        });
        let json = serde_json::to_string(&cp).expect("checkpoint should serialize");
        let back: SecurityCheckpoint =
            serde_json::from_str(&json).expect("checkpoint should deserialize");
        assert_eq!(
            cp, back,
            "round-trip through JSON should preserve the checkpoint exactly"
        );
    }

    #[test]
    fn security_harness_report_round_trips_through_json() {
        let report = SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: "run-1".into(),
            target: TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            scope: "scope".into(),
            findings: vec![SecurityFinding {
                id: "f1".into(),
                title: "t".into(),
                severity: "high".into(),
                vulnerability_class: "auth_authorization".into(),
                trust_boundary: "b".into(),
                entry_point: "e".into(),
                sink_or_decision: "s".into(),
                root_cause: "c".into(),
                affected_paths: vec!["p".into()],
                evidence: vec!["ev".into()],
                reachability: "r".into(),
                tenant_or_instance_impact: "i".into(),
                severity_rationale: "sr".into(),
                fix_recommendation: "fix".into(),
            }],
            rejected_candidates: vec![ValidationDecision {
                finding_id: "f2".into(),
                decision: ValidationDecisionKind::Rejected,
                evidence: "ev".into(),
                severity: Some("low".into()),
            }],
            gaps: vec![CoverageGap {
                area: "a".into(),
                reason: "r".into(),
                risk: "high".into(),
            }],
            dedupe_groups: vec![DedupeGroup {
                id: "dedupe-001".into(),
                root_cause: "rc".into(),
                primary_finding_id: "f1".into(),
                finding_ids: vec!["f1".into()],
                affected_paths: vec!["p".into()],
            }],
            trace_evidence: vec![TraceResult {
                finding_id: "f1".into(),
                reachable: true,
                severity_effect: "keeps".into(),
                evidence: vec!["te".into()],
            }],
            stage_history: vec![StageHistoryEntry {
                stage: SecurityHarnessStage::Report,
                status: "completed".into(),
                started_at: 10,
                finished_at: 20,
                summary: "ok".into(),
            }],
            class_coverage: vec![VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                class_name: "Authentication and authorization".into(),
                considered: true,
                applicable: true,
                hunted: true,
                skipped_reason: String::new(),
                high_risk_follow_up: false,
                checked_and_cleared: false,
                task_ids: vec!["t1".into()],
                evidence: vec!["ev".into()],
            }],
        };
        let json = serde_json::to_string(&report).expect("report should serialize");
        let back: SecurityHarnessReport =
            serde_json::from_str(&json).expect("report should deserialize");
        assert_eq!(
            report, back,
            "round-trip should preserve the report exactly"
        );
    }

    #[test]
    fn severity_rollup_buckets_match_panel_contract() {
        let f = |sev: &str| SecurityFinding {
            id: format!("f-{sev}"),
            severity: sev.into(),
            ..Default::default()
        };
        let r = SeverityRollup::from_findings(&[
            f("critical"),
            f("CRITICAL"), // case-insensitive
            f("high"),
            f("medium"),
            f("low"),
            f("info"),          // folds into low
            f("informational"), // folds into low
            f(""),              // unknown / blank: dropped, not bucketed
            f("nonsense"),      // unknown label: dropped
        ]);
        assert_eq!(r.critical, 2, "critical should be case-insensitive");
        assert_eq!(r.high, 1);
        assert_eq!(r.medium, 1);
        assert_eq!(
            r.low, 3,
            "low + info + informational all land in the low bucket — the panel only renders four severities"
        );
    }
}
