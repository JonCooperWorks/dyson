// ===========================================================================
// Security engineer staged research harness.
//
// The parent-facing tool remains `security_engineer`, but the implementation is
// no longer a single broad "review this repo" child agent. The orchestrator
// drives a staged harness:
//
//   Recon -> Hunt -> Validate -> Gapfill -> Dedupe -> Trace -> Feedback -> Report
//
// Each stage writes a durable JSON checkpoint under the Dyson workspace's kb/
// tree. In Swarm mode that path is mirrored by the existing state-file sync
// worker, so checkpoints survive instance recreate/rollout without adding a
// security-specific Swarm API.
// ===========================================================================

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::orchestrator::{OrchestratorConfig, OrchestratorHarness, OrchestratorInput};
use super::{ChildSpawn, spawn_child};
use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::Result;
use crate::llm::LlmClient;
use crate::message::{Artefact, ArtefactKind};
use crate::sandbox::Sandbox;
use crate::tool::{CheckpointEvent, Tool, ToolOutput};
use crate::workspace::WorkspaceHandle;

const DIRECT_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "search_files",
    "list_files",
    "ast_describe",
    "ast_query",
    "attack_surface_analyzer",
    "exploit_builder",
    "taint_trace",
    "dependency_scan",
];

const CHECKPOINT_PREFIX: &str = "kb/security-harness/checkpoints";
pub const SECURITY_HARNESS_SCHEMA_VERSION: u32 = 1;
pub const SECURITY_HARNESS_VERSION: &str = "security-harness-v1";
pub const DEFAULT_HUNT_BATCH_SIZE: usize = 4;

const STAGES: &[SecurityHarnessStage] = &[
    SecurityHarnessStage::Recon,
    SecurityHarnessStage::Hunt,
    SecurityHarnessStage::Validate,
    SecurityHarnessStage::Gapfill,
    SecurityHarnessStage::Dedupe,
    SecurityHarnessStage::Trace,
    SecurityHarnessStage::Feedback,
    SecurityHarnessStage::Report,
];

/// Build the OrchestratorConfig for the security_engineer role.
pub fn security_engineer_config() -> OrchestratorConfig {
    OrchestratorConfig {
        name: "security_engineer",
        description: "Runs a staged security research harness with durable checkpoints: recon, \
             narrow hunt batches, independent validation, gapfill, dedupe, reachability tracing, \
             feedback tasks, and schema-checked reporting. Use for scoped authorized reviews and \
             for resuming prior security_engineer checkpoints.",
        system_prompt: include_str!("prompts/security_engineer.md"),
        direct_tool_names: DIRECT_TOOLS,
        // The staged harness uses smaller per-stage child budgets internally;
        // this remains the advertised ceiling for legacy metadata/tests and
        // as an upper bound for any single security stage child.
        max_iterations: 80,
        max_tokens: 8192,
        injects_protocol: Some(include_str!("prompts/security_engineer_protocol.md")),
        inject_cheatsheets: true,
        emit_artefact: Some(ArtefactKind::SecurityReview),
        harness: Some(OrchestratorHarness::SecurityResearch),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn harness_stages() -> &'static [SecurityHarnessStage] {
    STAGES
}

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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Completed,
}

impl Default for TaskStatus {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityTask {
    pub id: String,
    pub attack_class: String,
    pub scope_hint: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SecurityFinding {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub root_cause: String,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub reachability: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDecisionKind {
    Confirmed,
    Rejected,
    NeedsMoreEvidence,
    Downgrade,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationDecision {
    pub finding_id: String,
    pub decision: ValidationDecisionKind,
    pub evidence: String,
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CoverageGap {
    pub area: String,
    pub reason: String,
    #[serde(default)]
    pub risk: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DedupeGroup {
    pub id: String,
    pub root_cause: String,
    pub primary_finding_id: String,
    #[serde(default)]
    pub finding_ids: Vec<String>,
    #[serde(default)]
    pub affected_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TraceResult {
    pub finding_id: String,
    pub reachable: bool,
    pub severity_effect: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetRef {
    pub repo_path: String,
    #[serde(default)]
    pub git_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelMetadata {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub active_cheatsheets: Vec<String>,
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
        checkpoint_path(&self.run_id)
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
}

#[derive(Debug, Deserialize)]
struct ReconStageOutput {
    architecture_context: String,
    #[serde(default)]
    tasks: Vec<SecurityTask>,
    #[serde(default)]
    coverage_gaps: Vec<CoverageGap>,
}

#[derive(Debug, Deserialize)]
struct HuntStageOutput {
    #[serde(default)]
    completed_task_ids: Vec<String>,
    #[serde(default)]
    findings: Vec<SecurityFinding>,
    #[serde(default)]
    gaps: Vec<CoverageGap>,
    #[serde(default)]
    follow_up_tasks: Vec<SecurityTask>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ValidateStageOutput {
    #[serde(default)]
    decisions: Vec<ValidationDecision>,
}

#[derive(Debug, Deserialize)]
struct TraceStageOutput {
    #[serde(default)]
    traces: Vec<TraceResult>,
}

pub(crate) struct SecurityHarnessRuntime {
    pub config_name: &'static str,
    pub provider: LlmProvider,
    pub model: String,
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Option<WorkspaceHandle>,
    pub parent_depth: u8,
    pub scoped_dir: Option<PathBuf>,
    pub parent_working_dir: PathBuf,
    pub all_tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub user_message: String,
    pub parsed: OrchestratorInput,
    pub activity: Option<crate::controller::ActivityHandle>,
    pub events: Option<crate::controller::http::SubagentEventBus>,
    pub parent_tool_id: Option<String>,
    pub emit_artefact: Option<ArtefactKind>,
    pub active_sheets: Vec<String>,
    pub max_tokens: u32,
}

pub(crate) async fn run_security_harness(rt: SecurityHarnessRuntime) -> Result<ToolOutput> {
    let started_at = std::time::SystemTime::now();
    let started_epoch = unix_seconds(started_at);
    let mut activity_token = rt.activity.as_ref().map(|a| {
        a.start(
            crate::controller::LANE_SUBAGENT,
            rt.config_name,
            &crate::controller::truncate_note(&rt.user_message, 80),
        )
    });

    let result = run_security_harness_inner(&rt, started_epoch).await;

    if let Some(tok) = activity_token.take() {
        let elapsed = unix_seconds(std::time::SystemTime::now()).saturating_sub(started_epoch);
        let suffix = format!("{elapsed}s");
        let status = match &result {
            Ok(out) if !out.is_error => crate::controller::ActivityStatus::Ok,
            _ => crate::controller::ActivityStatus::Err,
        };
        tok.finish(status, Some(&suffix));
    }

    result
}

async fn run_security_harness_inner(
    rt: &SecurityHarnessRuntime,
    started_epoch: u64,
) -> Result<ToolOutput> {
    let store = CheckpointStore::new(rt.workspace.clone(), rt.parent_working_dir.clone());
    let target_path = rt
        .scoped_dir
        .as_deref()
        .unwrap_or(rt.parent_working_dir.as_path())
        .display()
        .to_string();
    let target = TargetRef {
        git_ref: git_ref_for(rt.scoped_dir.as_deref().unwrap_or(&rt.parent_working_dir)),
        repo_path: target_path,
    };
    let scope = scope_for(&rt.parsed);
    let model = ModelMetadata {
        provider: provider_label(&rt.provider),
        model: rt.model.clone(),
        active_cheatsheets: rt.active_sheets.clone(),
    };

    let resume_requested = rt.parsed.resume
        || rt
            .parsed
            .run_id
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
        || rt.parsed.task.to_ascii_lowercase().contains("resume");

    let mut checkpoint = if resume_requested {
        match load_checkpoint_for_resume(&store, rt.parsed.run_id.as_deref(), &target.repo_path)
            .await
        {
            Ok(cp) => cp,
            Err(e) => return Ok(ToolOutput::error(e)),
        }
    } else {
        SecurityCheckpoint::new(make_run_id(), target, scope, model, started_epoch)
    };

    if checkpoint.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Ok(ToolOutput::error(format!(
            "checkpoint {} uses unsupported schema_version {}; expected {}",
            checkpoint.run_id, checkpoint.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        )));
    }
    if checkpoint.completed {
        return Ok(ToolOutput::error(format!(
            "checkpoint {} is already complete",
            checkpoint.run_id
        )));
    }

    checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
    if let Err(e) = store.save(&checkpoint).await {
        return Ok(ToolOutput::error(e));
    }

    let mut out = ToolOutput::success(String::new());
    out.checkpoints.push(CheckpointEvent {
        message: format!(
            "security_engineer: {} checkpoint {}",
            if resume_requested {
                "resuming"
            } else {
                "created"
            },
            checkpoint.run_id
        ),
        progress: Some(0.02),
    });

    for stage in STAGES {
        if stage_completed(&checkpoint, *stage) {
            continue;
        }
        checkpoint.current_stage = *stage;
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        if let Err(e) = store.save(&checkpoint).await {
            return Ok(ToolOutput::error(e));
        }
        out.checkpoints.push(CheckpointEvent {
            message: format!("security_engineer: {stage}"),
            progress: progress_for(*stage),
        });

        let stage_started = unix_seconds(std::time::SystemTime::now());
        let stage_result = match stage {
            SecurityHarnessStage::Recon => run_recon_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Hunt => run_hunt_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Validate => run_validate_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Gapfill => {
                run_gapfill_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Dedupe => {
                run_dedupe_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Trace => run_trace_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Feedback => {
                run_feedback_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Report => run_report_stage(rt, &mut checkpoint).await,
        };

        match stage_result {
            Ok(Some(stage_output)) => merge_stage_tool_output(&mut out, stage_output),
            Ok(None) => {}
            Err(e) => {
                checkpoint.report_validation_state = ReportValidationState {
                    status: "failed".into(),
                    errors: vec![e.clone()],
                };
                checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
                let _ = store.save(&checkpoint).await;
                return Ok(ToolOutput::error(format!(
                    "security_engineer {stage} failed: {e}. checkpoint={}",
                    checkpoint.run_id
                )));
            }
        }

        let stage_finished = unix_seconds(std::time::SystemTime::now());
        checkpoint.stage_history.push(StageHistoryEntry {
            stage: *stage,
            status: "completed".into(),
            started_at: stage_started,
            finished_at: stage_finished,
            summary: stage_summary(&checkpoint, *stage),
        });
        checkpoint.updated_at = stage_finished;
        if let Err(e) = store.save(&checkpoint).await {
            return Ok(ToolOutput::error(e));
        }

        if should_stop_after(&rt.parsed, *stage) {
            out.content = format!(
                "security_engineer checkpoint saved after {stage}. run_id={} path={}. Resume with {{\"task\":\"resume security review\",\"resume\":true,\"run_id\":\"{}\"}}.",
                checkpoint.run_id,
                checkpoint.checkpoint_path(),
                checkpoint.run_id,
            );
            return Ok(out);
        }
    }

    checkpoint.completed = true;
    checkpoint.current_stage = SecurityHarnessStage::Report;
    checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
    if let Err(e) = store.save(&checkpoint).await {
        return Ok(ToolOutput::error(e));
    }

    let report = checkpoint
        .report_draft
        .clone()
        .unwrap_or_else(|| report_from_checkpoint(&checkpoint));
    let report_json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into());
    let elapsed = checkpoint.updated_at.saturating_sub(started_epoch);
    out.content = format!(
        "# Security Harness Report: {}\n\n```json\n{}\n```\n",
        report.target.repo_path, report_json
    );
    out.checkpoints.push(CheckpointEvent {
        message: format!(
            "security_engineer: completed {} in {}s",
            checkpoint.run_id, elapsed
        ),
        progress: Some(1.0),
    });

    if let Some(kind) = rt.emit_artefact {
        let title = format!(
            "Security harness: {}",
            target_name_for(&report.target.repo_path)
        );
        let metadata = serde_json::json!({
            "run_id": checkpoint.run_id,
            "harness_version": SECURITY_HARNESS_VERSION,
            "schema_version": SECURITY_HARNESS_SCHEMA_VERSION,
            "target_path": report.target.repo_path,
            "provider": provider_label(&rt.provider),
            "model": rt.model,
            "checkpoint_path": checkpoint.checkpoint_path(),
            "stage_count": checkpoint.stage_history.len(),
        });
        out.artefacts
            .push(Artefact::markdown(kind, title, out.content.clone()).with_metadata(metadata));
    }

    Ok(out)
}

async fn run_recon_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("prompts/security_engineer_recon.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Recon, prompt, checkpoint, 12).await?;
    let recon: ReconStageOutput = parse_stage_json(&raw)?;
    checkpoint.architecture_context = recon.architecture_context;
    let mut tasks = recon.tasks;
    if tasks.is_empty() {
        tasks.push(SecurityTask {
            id: "hunt-001".into(),
            attack_class: "security_boundary".into(),
            scope_hint: checkpoint.scope.clone(),
            status: TaskStatus::Pending,
            rationale: "fallback task because recon returned no tasks".into(),
        });
    }
    normalize_task_ids(&mut tasks, "hunt");
    checkpoint.pending_tasks.extend(tasks);
    checkpoint.coverage_gaps.extend(recon.coverage_gaps);
    Ok(Some(stage_out))
}

async fn run_hunt_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let batch = next_hunt_batch(checkpoint, DEFAULT_HUNT_BATCH_SIZE);
    if batch.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("prompts/security_engineer_hunt.md");
    let mut checkpoint_for_prompt = checkpoint.clone();
    checkpoint_for_prompt.pending_tasks = batch.clone();
    let (raw, stage_out) = spawn_stage_with_checkpoint(
        rt,
        SecurityHarnessStage::Hunt,
        prompt,
        &checkpoint_for_prompt,
        28,
    )
    .await?;
    let hunt: HuntStageOutput = parse_stage_json(&raw)?;
    let completed_ids: BTreeSet<String> = if hunt.completed_task_ids.is_empty() {
        batch.iter().map(|t| t.id.clone()).collect()
    } else {
        hunt.completed_task_ids.into_iter().collect()
    };
    complete_tasks(checkpoint, &completed_ids);
    checkpoint.findings_so_far.extend(
        hunt.findings
            .into_iter()
            .filter(|finding| !finding.id.trim().is_empty()),
    );
    checkpoint.coverage_gaps.extend(hunt.gaps);
    let mut followups = hunt.follow_up_tasks;
    normalize_task_ids(&mut followups, "gap");
    checkpoint.gapfill_tasks.extend(followups.clone());
    checkpoint.pending_tasks.extend(followups);
    Ok(Some(stage_out))
}

async fn run_validate_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    if checkpoint.findings_so_far.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("prompts/security_engineer_validate.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Validate, prompt, checkpoint, 16).await?;
    let validate = parse_validate_output(&raw, &checkpoint.findings_so_far)?;
    checkpoint
        .validation_decisions_so_far
        .extend(validate.decisions);
    Ok(Some(stage_out))
}

fn run_gapfill_stage(checkpoint: &mut SecurityCheckpoint) {
    let existing: BTreeSet<String> = checkpoint
        .pending_tasks
        .iter()
        .chain(checkpoint.completed_tasks.iter())
        .map(|t| t.id.clone())
        .collect();
    let mut next_id = checkpoint.pending_tasks.len() + checkpoint.completed_tasks.len() + 1;
    let mut additions = Vec::new();
    for gap in &checkpoint.coverage_gaps {
        let risk = gap.risk.to_ascii_lowercase();
        if !(risk.contains("high") || risk.contains("critical")) {
            continue;
        }
        let id = format!("gapfill-{next_id:03}");
        next_id += 1;
        if existing.contains(&id) {
            continue;
        }
        additions.push(SecurityTask {
            id,
            attack_class: "gapfill".into(),
            scope_hint: gap.area.clone(),
            status: TaskStatus::Pending,
            rationale: gap.reason.clone(),
        });
    }
    checkpoint.gapfill_tasks.extend(additions.clone());
    checkpoint.pending_tasks.extend(additions);
}

fn run_dedupe_stage(checkpoint: &mut SecurityCheckpoint) {
    checkpoint.dedupe_groups_so_far = dedupe_findings(&checkpoint.findings_so_far);
}

async fn run_trace_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let confirmed: Vec<&ValidationDecision> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Confirmed)
        .collect();
    if confirmed.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("prompts/security_engineer_trace.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Trace, prompt, checkpoint, 16).await?;
    let traces: TraceStageOutput = parse_stage_json(&raw)?;
    checkpoint.trace_results_so_far.extend(traces.traces);
    Ok(Some(stage_out))
}

fn run_feedback_stage(checkpoint: &mut SecurityCheckpoint) {
    let mut next = checkpoint.pending_tasks.len() + checkpoint.completed_tasks.len() + 1;
    let mut existing: BTreeSet<String> = checkpoint
        .pending_tasks
        .iter()
        .chain(checkpoint.completed_tasks.iter())
        .map(|t| t.scope_hint.clone())
        .collect();
    for trace in &checkpoint.trace_results_so_far {
        if !trace.reachable {
            continue;
        }
        let Some(finding) = checkpoint
            .findings_so_far
            .iter()
            .find(|finding| finding.id == trace.finding_id)
        else {
            continue;
        };
        for path in &finding.affected_paths {
            if !existing.insert(path.clone()) {
                continue;
            }
            checkpoint.pending_tasks.push(SecurityTask {
                id: format!("feedback-{next:03}"),
                attack_class: format!("consumer_of_{}", finding.root_cause),
                scope_hint: path.clone(),
                status: TaskStatus::Pending,
                rationale: "reachable shared-component finding; inspect consumer path".into(),
            });
            next += 1;
        }
    }
}

async fn run_report_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("prompts/security_engineer_report.md");
    let (raw, mut stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Report, prompt, checkpoint, 10).await?;
    let parsed = match parse_report_output(&raw) {
        Ok(report) => report,
        Err(first_err) => {
            checkpoint.report_validation_state = ReportValidationState {
                status: "repairing".into(),
                errors: vec![first_err.clone()],
            };
            let repair_prompt = include_str!("prompts/security_engineer_report_repair.md");
            let (repair_raw, repair_out) = spawn_stage(
                rt,
                SecurityHarnessStage::Report,
                repair_prompt,
                checkpoint,
                6,
            )
            .await?;
            merge_stage_tool_output(&mut stage_out, repair_out);
            parse_report_output(&repair_raw).map_err(|second_err| {
                format!("report schema validation failed after repair: {first_err}; {second_err}")
            })?
        }
    };
    checkpoint.report_validation_state = ReportValidationState {
        status: "valid".into(),
        errors: Vec::new(),
    };
    checkpoint.report_draft = Some(parsed);
    Ok(Some(stage_out))
}

async fn spawn_stage(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    checkpoint: &SecurityCheckpoint,
    max_iterations: usize,
) -> std::result::Result<(String, ToolOutput), String> {
    spawn_stage_with_checkpoint(rt, stage, prompt, checkpoint, max_iterations).await
}

async fn spawn_stage_with_checkpoint(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    checkpoint: &SecurityCheckpoint,
    max_iterations: usize,
) -> std::result::Result<(String, ToolOutput), String> {
    let checkpoint_json = serde_json::to_string_pretty(checkpoint)
        .map_err(|e| format!("serialize checkpoint for {stage}: {e}"))?;
    let system_prompt = format!("{}\n\n{}", rt.system_prompt, prompt);
    let stage_message = format!(
        "Parent request:\n{}\n\nCurrent durable checkpoint JSON:\n```json\n{}\n```\n",
        rt.user_message, checkpoint_json
    );
    let settings = AgentSettings {
        model: rt.model.clone(),
        max_iterations,
        max_tokens: rt.max_tokens,
        system_prompt,
        provider: rt.provider.clone(),
        ..AgentSettings::default()
    };
    let out = spawn_child(ChildSpawn {
        name: stage.as_str(),
        settings,
        inherited_tools: rt.all_tools.clone(),
        sandbox: Arc::clone(&rt.sandbox),
        workspace: rt.workspace.clone(),
        client: rt.client.clone(),
        parent_depth: rt.parent_depth,
        working_dir: rt.scoped_dir.clone(),
        user_message: stage_message,
        activity: rt.activity.clone(),
        events: rt.events.clone(),
        parent_tool_id: rt.parent_tool_id.clone(),
    })
    .await
    .map_err(|e| e.to_string())?;
    if out.is_error {
        return Err(out.content);
    }
    Ok((out.content.clone(), out))
}

fn merge_stage_tool_output(target: &mut ToolOutput, mut stage: ToolOutput) {
    target.checkpoints.append(&mut stage.checkpoints);
    target.artefacts.append(&mut stage.artefacts);
    let Some(stage_meta) = stage.metadata.take() else {
        return;
    };
    let mut meta = target.metadata.take().unwrap_or_else(|| {
        serde_json::json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "llm_calls": 0,
        })
    });
    for key in ["input_tokens", "output_tokens", "llm_calls"] {
        let current = meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let add = stage_meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        meta[key] = serde_json::json!(current + add);
    }
    target.metadata = Some(meta);
}

pub fn validate_report_json(
    value: &serde_json::Value,
) -> std::result::Result<SecurityHarnessReport, String> {
    let report: SecurityHarnessReport =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if report.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {}; expected {}",
            report.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        ));
    }
    if report.run_id.trim().is_empty() {
        return Err("run_id is required".into());
    }
    if report.target.repo_path.trim().is_empty() {
        return Err("target.repo_path is required".into());
    }
    for finding in &report.findings {
        if finding.id.trim().is_empty()
            || finding.title.trim().is_empty()
            || finding.root_cause.trim().is_empty()
        {
            return Err("findings require id, title, and root_cause".into());
        }
    }
    Ok(report)
}

pub(crate) fn parse_report_output(raw: &str) -> std::result::Result<SecurityHarnessReport, String> {
    let value = parse_json_value(raw)?;
    validate_report_json(&value)
}

pub(crate) fn parse_validate_output(
    raw: &str,
    findings: &[SecurityFinding],
) -> std::result::Result<ValidateStageOutput, String> {
    let value = parse_json_value(raw)?;
    if value.get("findings").is_some() {
        return Err("validator output must not include new findings".into());
    }
    let parsed: ValidateStageOutput = serde_json::from_value(value).map_err(|e| e.to_string())?;
    let known: BTreeSet<&str> = findings.iter().map(|f| f.id.as_str()).collect();
    for decision in &parsed.decisions {
        if !known.contains(decision.finding_id.as_str()) {
            return Err(format!(
                "validator referenced unknown finding_id {}",
                decision.finding_id
            ));
        }
    }
    Ok(parsed)
}

fn parse_stage_json<T: for<'de> Deserialize<'de>>(raw: &str) -> std::result::Result<T, String> {
    let value = parse_json_value(raw)?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn parse_json_value(raw: &str) -> std::result::Result<serde_json::Value, String> {
    let candidate =
        extract_json(raw).ok_or_else(|| "no JSON object found in stage output".to_string())?;
    serde_json::from_str(candidate).map_err(|e| format!("invalid JSON: {e}"))
}

fn extract_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + "```json".len()..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end > start).then(|| trimmed[start..=end].trim())
}

fn next_hunt_batch(checkpoint: &SecurityCheckpoint, batch_size: usize) -> Vec<SecurityTask> {
    checkpoint
        .pending_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Pending)
        .take(batch_size)
        .cloned()
        .collect()
}

fn complete_tasks(checkpoint: &mut SecurityCheckpoint, completed_ids: &BTreeSet<String>) {
    let mut remaining = Vec::new();
    for mut task in checkpoint.pending_tasks.drain(..) {
        if completed_ids.contains(&task.id) {
            task.status = TaskStatus::Completed;
            checkpoint.completed_tasks.push(task);
        } else {
            remaining.push(task);
        }
    }
    checkpoint.pending_tasks = remaining;
}

fn normalize_task_ids(tasks: &mut [SecurityTask], prefix: &str) {
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.id.trim().is_empty() {
            task.id = format!("{prefix}-{:03}", idx + 1);
        }
    }
}

pub fn dedupe_findings(findings: &[SecurityFinding]) -> Vec<DedupeGroup> {
    let mut by_root: BTreeMap<String, Vec<&SecurityFinding>> = BTreeMap::new();
    for finding in findings {
        let root = if finding.root_cause.trim().is_empty() {
            finding.title.clone()
        } else {
            finding.root_cause.clone()
        };
        by_root.entry(root).or_default().push(finding);
    }
    by_root
        .into_iter()
        .enumerate()
        .map(|(idx, (root_cause, group))| {
            let primary = group.first().map(|f| f.id.clone()).unwrap_or_default();
            let mut affected = BTreeSet::new();
            for finding in &group {
                affected.extend(finding.affected_paths.iter().cloned());
            }
            DedupeGroup {
                id: format!("dedupe-{:03}", idx + 1),
                root_cause,
                primary_finding_id: primary,
                finding_ids: group.iter().map(|f| f.id.clone()).collect(),
                affected_paths: affected.into_iter().collect(),
            }
        })
        .collect()
}

fn report_from_checkpoint(checkpoint: &SecurityCheckpoint) -> SecurityHarnessReport {
    let rejected_candidates = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Rejected)
        .cloned()
        .collect();
    SecurityHarnessReport {
        schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
        run_id: checkpoint.run_id.clone(),
        target: checkpoint.target.clone(),
        scope: checkpoint.scope.clone(),
        findings: checkpoint.findings_so_far.clone(),
        rejected_candidates,
        coverage: checkpoint.coverage_gaps.clone(),
        gaps: checkpoint.coverage_gaps.clone(),
        dedupe_groups: checkpoint.dedupe_groups_so_far.clone(),
        trace_evidence: checkpoint.trace_results_so_far.clone(),
        stage_history: checkpoint.stage_history.clone(),
    }
}

fn stage_completed(checkpoint: &SecurityCheckpoint, stage: SecurityHarnessStage) -> bool {
    checkpoint
        .stage_history
        .iter()
        .any(|entry| entry.stage == stage && entry.status == "completed")
}

fn stage_summary(checkpoint: &SecurityCheckpoint, stage: SecurityHarnessStage) -> String {
    match stage {
        SecurityHarnessStage::Recon => format!(
            "{} pending hunt tasks; {} gaps",
            checkpoint.pending_tasks.len(),
            checkpoint.coverage_gaps.len()
        ),
        SecurityHarnessStage::Hunt => format!(
            "{} completed tasks; {} findings",
            checkpoint.completed_tasks.len(),
            checkpoint.findings_so_far.len()
        ),
        SecurityHarnessStage::Validate => {
            format!(
                "{} validation decisions",
                checkpoint.validation_decisions_so_far.len()
            )
        }
        SecurityHarnessStage::Gapfill => {
            format!("{} gapfill tasks", checkpoint.gapfill_tasks.len())
        }
        SecurityHarnessStage::Dedupe => {
            format!("{} dedupe groups", checkpoint.dedupe_groups_so_far.len())
        }
        SecurityHarnessStage::Trace => {
            format!("{} trace results", checkpoint.trace_results_so_far.len())
        }
        SecurityHarnessStage::Feedback => {
            format!("{} pending feedback tasks", checkpoint.pending_tasks.len())
        }
        SecurityHarnessStage::Report => checkpoint.report_validation_state.status.clone(),
    }
}

fn progress_for(stage: SecurityHarnessStage) -> Option<f32> {
    Some(match stage {
        SecurityHarnessStage::Recon => 0.10,
        SecurityHarnessStage::Hunt => 0.25,
        SecurityHarnessStage::Validate => 0.45,
        SecurityHarnessStage::Gapfill => 0.58,
        SecurityHarnessStage::Dedupe => 0.66,
        SecurityHarnessStage::Trace => 0.76,
        SecurityHarnessStage::Feedback => 0.86,
        SecurityHarnessStage::Report => 0.94,
    })
}

fn should_stop_after(parsed: &OrchestratorInput, stage: SecurityHarnessStage) -> bool {
    parsed
        .stop_after_stage
        .as_deref()
        .and_then(SecurityHarnessStage::parse)
        == Some(stage)
}

fn scope_for(parsed: &OrchestratorInput) -> String {
    let mut parts = Vec::new();
    if !parsed.path.trim().is_empty() {
        parts.push(format!("path={}", parsed.path.trim()));
    }
    if !parsed.context.trim().is_empty() {
        parts.push(format!("context={}", parsed.context.trim()));
    }
    parts.push(format!("task={}", parsed.task.trim()));
    parts.join("\n")
}

fn make_run_id() -> String {
    format!(
        "sec-{}-{}",
        unix_seconds(std::time::SystemTime::now()),
        std::process::id()
    )
}

fn checkpoint_path(run_id: &str) -> String {
    format!("{CHECKPOINT_PREFIX}/{run_id}.json")
}

fn unix_seconds(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn provider_label(provider: &LlmProvider) -> String {
    format!("{provider:?}")
}

fn target_name_for(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("target")
        .to_string()
}

fn git_ref_for(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

struct CheckpointStore {
    workspace: Option<WorkspaceHandle>,
    fallback_dir: PathBuf,
}

impl CheckpointStore {
    fn new(workspace: Option<WorkspaceHandle>, working_dir: PathBuf) -> Self {
        Self {
            workspace,
            fallback_dir: working_dir
                .join(".dyson")
                .join("security-harness")
                .join("checkpoints"),
        }
    }

    async fn save(&self, checkpoint: &SecurityCheckpoint) -> std::result::Result<(), String> {
        let body = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
        if let Some(workspace) = &self.workspace {
            let mut guard = workspace.write().await;
            guard.set(&checkpoint.checkpoint_path(), &body);
            guard.save().map_err(|e| e.to_string())?;
            return Ok(());
        }
        std::fs::create_dir_all(&self.fallback_dir).map_err(|e| {
            format!(
                "cannot create checkpoint dir {}: {e}",
                self.fallback_dir.display()
            )
        })?;
        std::fs::write(
            self.fallback_dir
                .join(format!("{}.json", checkpoint.run_id)),
            body,
        )
        .map_err(|e| format!("cannot write checkpoint: {e}"))
    }

    async fn load_exact(&self, run_id: &str) -> std::result::Result<SecurityCheckpoint, String> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let path = checkpoint_path(run_id);
            let Some(body) = guard.get(&path) else {
                let disk_root = guard
                    .programs_dir()
                    .and_then(|programs| programs.parent().map(std::path::Path::to_path_buf));
                drop(guard);
                if let Some(root) = disk_root {
                    let path = root.join(checkpoint_path(run_id));
                    let body = std::fs::read_to_string(&path).map_err(|_| {
                        format!("checkpoint {run_id} not found at {}", path.display())
                    })?;
                    return parse_checkpoint(&body);
                }
                return Err(format!("checkpoint {run_id} not found"));
            };
            return parse_checkpoint(&body);
        }
        let path = self.fallback_dir.join(format!("{run_id}.json"));
        let body = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read checkpoint {}: {e}", path.display()))?;
        parse_checkpoint(&body)
    }

    async fn list(&self) -> Vec<SecurityCheckpoint> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let mut checkpoints: Vec<SecurityCheckpoint> = guard
                .list_files()
                .into_iter()
                .filter(|p| p.starts_with(CHECKPOINT_PREFIX) && p.ends_with(".json"))
                .filter_map(|p| guard.get(&p).and_then(|body| parse_checkpoint(&body).ok()))
                .collect();
            let disk_root = guard
                .programs_dir()
                .and_then(|programs| programs.parent().map(std::path::Path::to_path_buf));
            drop(guard);
            if let Some(root) = disk_root {
                checkpoints.extend(read_checkpoint_dir(root.join(CHECKPOINT_PREFIX)));
                checkpoints.sort_by(|a, b| a.run_id.cmp(&b.run_id));
                checkpoints.dedup_by(|a, b| a.run_id == b.run_id);
            }
            return checkpoints;
        }
        read_checkpoint_dir(self.fallback_dir.clone())
    }
}

fn read_checkpoint_dir(path: PathBuf) -> Vec<SecurityCheckpoint> {
    let Ok(entries) = std::fs::read_dir(&path) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let body = std::fs::read_to_string(path).ok()?;
            parse_checkpoint(&body).ok()
        })
        .collect()
}

fn parse_checkpoint(body: &str) -> std::result::Result<SecurityCheckpoint, String> {
    let checkpoint: SecurityCheckpoint = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if checkpoint.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported checkpoint schema_version {}; expected {}",
            checkpoint.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        ));
    }
    if checkpoint.harness_version != SECURITY_HARNESS_VERSION {
        return Err(format!(
            "unsupported checkpoint harness_version {}; expected {}",
            checkpoint.harness_version, SECURITY_HARNESS_VERSION
        ));
    }
    Ok(checkpoint)
}

async fn load_checkpoint_for_resume(
    store: &CheckpointStore,
    run_id: Option<&str>,
    target_path: &str,
) -> std::result::Result<SecurityCheckpoint, String> {
    if let Some(run_id) = run_id.filter(|s| !s.trim().is_empty()) {
        return store.load_exact(run_id.trim()).await;
    }
    let mut matches: Vec<SecurityCheckpoint> = store
        .list()
        .await
        .into_iter()
        .filter(|cp| !cp.completed && cp.target.repo_path == target_path)
        .collect();
    matches.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    match matches.len() {
        0 => Err(format!(
            "no incomplete security_engineer checkpoint found for {target_path}"
        )),
        1 => Ok(matches.remove(0)),
        _ => {
            let list = matches
                .iter()
                .take(8)
                .map(|cp| {
                    format!(
                        "- {} stage={} updated_at={}",
                        cp.run_id, cp.current_stage, cp.updated_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "multiple incomplete security_engineer checkpoints found; rerun with run_id:\n{list}"
            ))
        }
    }
}
