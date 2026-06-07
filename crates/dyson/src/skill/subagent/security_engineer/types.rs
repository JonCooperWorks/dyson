// ===========================================================================
// Data types for the security_engineer staged harness.
//
// Everything here is plain data: stage enum, task/finding/decision structs,
// coverage tracking, the durable SecurityCheckpoint shape, the four stage
// output structs the LLM emits, and the report struct rendered into Markdown.
//
// LLM output is best-effort. Every Deserialize field carries
// `#[serde(default)]` so a model that omits, mis-types, or merely renames a
// field cannot poison the harness — downstream code already tolerates empty
// ids/strings (normalize_task_ids backfills, dedupe falls back to title, etc.).
// ===========================================================================

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
    pub coverage: Vec<CoverageGap>,
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

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
pub(crate) struct ValidateStageOutput {
    #[serde(default)]
    pub decisions: Vec<ValidationDecision>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TraceStageOutput {
    #[serde(default)]
    pub traces: Vec<TraceResult>,
}
