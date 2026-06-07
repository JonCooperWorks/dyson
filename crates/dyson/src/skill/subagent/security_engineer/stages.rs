// ===========================================================================
// Stage runners for the security_engineer harness.
//
// One async (or sync, for the bookkeeping-only stages) function per stage:
//
//   Recon -> Hunt -> Validate -> Gapfill -> Dedupe -> Trace -> Feedback -> Report
//
// Each stage reads/writes the shared SecurityCheckpoint; the harness loop in
// mod.rs persists the checkpoint between stages.  Stage runners return
// `Option<ToolOutput>` so bookkeeping-only stages (Gapfill/Dedupe/Feedback)
// can avoid emitting empty stage outputs.
// ===========================================================================

use std::collections::BTreeSet;

use crate::tool::{CheckpointEvent, ToolOutput};

use super::checkpoint::{CheckpointStore, unix_seconds};
use super::parse::{parse_report_output, parse_stage_json, parse_validate_output};
use super::report::{report_checkpoint_for_prompt, report_from_checkpoint};
use super::runtime::{
    SecurityHarnessRuntime, merge_stage_tool_output, spawn_stage, spawn_stage_with_checkpoint,
};
use super::stack::{StackSpecialist, prune_inapplicable_class_tasks, stack_specialists};
use super::taxonomy::{
    build_class_coverage, canonical_vulnerability_class, canonicalize_findings, canonicalize_tasks,
    class_specialist_briefing, ensure_taxonomy_hunt_tasks, mark_hunted_class_coverage,
    update_class_coverage_task_ids,
};
use super::types::{
    CoverageGap, HuntStageOutput, ReconStageOutput, ReportValidationState, SecurityCheckpoint,
    SecurityHarnessReport, SecurityHarnessStage, SecurityTask, TaskStatus, TraceStageOutput,
    ValidationDecisionKind,
};
use super::{
    HUNT_MAX_ITERATIONS, HUNT_SPECIALIST_BACKSTOP, RECON_MAX_ITERATIONS, TRACE_MAX_ITERATIONS,
    VALIDATE_MAX_ITERATIONS,
};

const HUNT_CONCURRENCY: usize = 6;

pub(super) async fn run_recon_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("../prompts/security_engineer_recon.md");
    let (raw, stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Recon,
        prompt,
        checkpoint,
        RECON_MAX_ITERATIONS,
    )
    .await?;
    // Parse failure is non-fatal. If a thorough model exhausts its
    // iteration cap, the agent loop's summarize-on-cap path returns prose
    // instead of JSON; if a weaker model emits malformed structure, the
    // permissive ReconStageOutput defaults already absorb most damage.
    // Either way, ensure_taxonomy_hunt_tasks queues every class
    // unconditionally below, so the recon→hunt transition cannot be
    // blocked by a single bad stage output.
    let recon: ReconStageOutput = parse_stage_json(&raw).unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "recon stage output did not parse as JSON; using empty recon — \
             hunt will still fan out via taxonomy fallback"
        );
        ReconStageOutput::default()
    });
    checkpoint.architecture_context = recon.architecture_context;
    checkpoint.class_coverage = build_class_coverage(
        &checkpoint.scope,
        &checkpoint.architecture_context,
        recon.class_coverage,
    );
    let mut tasks = recon.tasks;
    canonicalize_tasks(&mut tasks);
    ensure_taxonomy_hunt_tasks(checkpoint, &mut tasks);
    normalize_task_ids(&mut tasks, "hunt");
    update_class_coverage_task_ids(&mut checkpoint.class_coverage, &tasks);
    checkpoint.pending_tasks.extend(tasks);
    checkpoint.coverage_gaps.extend(recon.coverage_gaps);
    Ok(Some(stage_out))
}

pub(super) async fn run_hunt_stage(
    rt: &SecurityHarnessRuntime,
    store: &CheckpointStore,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("../prompts/security_engineer_hunt.md");
    let mut aggregate = ToolOutput::success(String::new());
    let mut ran_batch = false;

    // Deterministic stack detection drives specialist selection.  Detection
    // runs against the effective review root (scoped path if provided).
    let target_root = rt
        .scoped_dir
        .as_deref()
        .unwrap_or(rt.parent_working_dir.as_path());
    let detection = crate::skill::subagent::repo_detect::detect_repo(target_root);

    // Conservative pruning: drop only classes that are provably moot for the
    // detected stack.  Everything behavior-dependent still runs.
    prune_inapplicable_class_tasks(checkpoint, &detection);

    // Class specialists, dispatched in concurrency-bounded waves.  Each wave
    // hunts every currently-pending class in parallel (one specialist per
    // class, briefed with that class's evidence/detector/AST patterns); the
    // follow-up tasks a wave produces (e.g. consumer_path_review) feed the
    // next wave.  Specialist count is work-list-driven; the backstop only
    // guards a runaway recon.
    let mut total_spawned = 0usize;
    loop {
        if total_spawned >= HUNT_SPECIALIST_BACKSTOP {
            break;
        }
        let mut dispatches = Vec::new();
        for class_id in distinct_pending_classes(checkpoint) {
            if total_spawned + dispatches.len() >= HUNT_SPECIALIST_BACKSTOP {
                break;
            }
            let batch = pending_tasks_for_class(checkpoint, &class_id);
            if batch.is_empty() {
                continue;
            }
            let mut cp = checkpoint.clone();
            cp.pending_tasks = batch.clone();
            let stage_prompt = match class_specialist_briefing(&class_id) {
                Some(briefing) => format!("{prompt}\n\n{briefing}"),
                None => prompt.to_string(),
            };
            dispatches.push(HuntDispatch {
                label: class_id,
                stage_prompt,
                checkpoint: cp,
                batch_ids: batch.iter().map(|t| t.id.clone()).collect(),
                is_class: true,
            });
        }
        if dispatches.is_empty() {
            break;
        }
        ran_batch = true;
        total_spawned += dispatches.len();
        let first_err = fold_hunt_wave(
            checkpoint,
            &mut aggregate,
            dispatch_hunts(rt, dispatches).await,
        );
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        store.save(checkpoint).await?;
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    // Framework/language specialists: each stack briefing matched by
    // deterministic detection, spawned as its own hunter briefed with only its
    // own reference material.  Augments the class specialists with
    // idiomatic-sink coverage without bloating any shared prompt.  One wave.
    let stack = stack_specialists(&detection);
    if !stack.is_empty() {
        ran_batch = true;
        let dispatches = stack
            .into_iter()
            .map(|spec: StackSpecialist| {
                let mut cp = checkpoint.clone();
                cp.pending_tasks = vec![SecurityTask {
                    id: spec.task_id,
                    attack_class: spec.label.clone(),
                    scope_hint: spec.scope_hint,
                    status: TaskStatus::Pending,
                    rationale: String::new(),
                }];
                HuntDispatch {
                    label: spec.label,
                    stage_prompt: format!("{prompt}\n\n{}", spec.briefing),
                    checkpoint: cp,
                    batch_ids: Vec::new(),
                    is_class: false,
                }
            })
            .collect::<Vec<_>>();
        let first_err = fold_hunt_wave(
            checkpoint,
            &mut aggregate,
            dispatch_hunts(rt, dispatches).await,
        );
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        store.save(checkpoint).await?;
        if let Some(e) = first_err {
            return Err(e);
        }
    }

    if ran_batch {
        Ok(Some(aggregate))
    } else {
        Ok(None)
    }
}

/// One specialist hunter to dispatch: a fully-prepared child prompt plus the
/// checkpoint snapshot it sees.  `batch_ids` are the pending task ids a class
/// specialist owns (empty for stack specialists, which hunt a synthetic task).
pub(super) struct HuntDispatch {
    pub label: String,
    pub stage_prompt: String,
    pub checkpoint: SecurityCheckpoint,
    pub batch_ids: Vec<String>,
    pub is_class: bool,
}

/// One dispatched specialist paired with its child result (`(raw_json,
/// tool_output)` on success, error string on failure).
pub(super) type HuntOutcome = (
    HuntDispatch,
    std::result::Result<(String, ToolOutput), String>,
);

/// Distinct attack classes among the still-pending tasks, in first-seen order.
pub(super) fn distinct_pending_classes(checkpoint: &SecurityCheckpoint) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for task in &checkpoint.pending_tasks {
        if task.status == TaskStatus::Pending && seen.insert(task.attack_class.clone()) {
            out.push(task.attack_class.clone());
        }
    }
    out
}

/// Run a wave of specialist hunters concurrently, bounded by
/// [`HUNT_CONCURRENCY`].  Each child is independent (its own checkpoint
/// snapshot), so they fan out without shared mutable state; results are folded
/// sequentially afterward.  buffer_unordered polls all futures on this task —
/// real LLM concurrency is bounded by the client's own limiter underneath.
pub(super) async fn dispatch_hunts(
    rt: &SecurityHarnessRuntime,
    dispatches: Vec<HuntDispatch>,
) -> Vec<HuntOutcome> {
    use futures_util::stream::StreamExt;
    futures_util::stream::iter(dispatches.into_iter().map(|d| async move {
        let res = spawn_stage_with_checkpoint(
            rt,
            SecurityHarnessStage::Hunt,
            &d.stage_prompt,
            &d.checkpoint,
            HUNT_MAX_ITERATIONS,
        )
        .await;
        (d, res)
    }))
    .buffer_unordered(HUNT_CONCURRENCY)
    .collect()
    .await
}

/// Fold a completed wave's results into the checkpoint.  Returns the first
/// child error (if any) after folding every success, so partial progress is
/// still checkpointed before the stage fails.
pub(super) fn fold_hunt_wave(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    results: Vec<HuntOutcome>,
) -> Option<String> {
    let mut first_err = None;
    for (d, res) in results {
        match res {
            Ok((raw, stage_out)) => {
                if let Err(e) = fold_hunt_result(checkpoint, aggregate, &d, raw, stage_out) {
                    first_err.get_or_insert(e);
                }
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    first_err
}

/// Merge one specialist's output into the checkpoint: findings, coverage gaps,
/// follow-up tasks, and (for class specialists) task completion + class
/// coverage bookkeeping.
pub(super) fn fold_hunt_result(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    d: &HuntDispatch,
    raw: String,
    stage_out: ToolOutput,
) -> std::result::Result<(), String> {
    merge_stage_tool_output(aggregate, stage_out);
    let hunt: HuntStageOutput = parse_stage_json(&raw)?;
    if d.is_class {
        let completed_ids: BTreeSet<String> = if hunt.completed_task_ids.is_empty() {
            d.batch_ids.iter().cloned().collect()
        } else {
            hunt.completed_task_ids.iter().cloned().collect()
        };
        complete_tasks(checkpoint, &completed_ids);
    }
    let mut findings = hunt
        .findings
        .into_iter()
        .filter(|finding| !finding.id.trim().is_empty())
        .collect::<Vec<_>>();
    canonicalize_findings(&mut findings);
    checkpoint.findings_so_far.extend(findings);
    checkpoint.coverage_gaps.extend(hunt.gaps);
    let mut followups = hunt.follow_up_tasks;
    canonicalize_tasks(&mut followups);
    normalize_task_ids(&mut followups, "gap");
    if d.is_class {
        update_class_coverage_task_ids(&mut checkpoint.class_coverage, &followups);
    }
    checkpoint.gapfill_tasks.extend(followups.clone());
    checkpoint.pending_tasks.extend(followups);
    if d.is_class {
        mark_hunted_class_coverage(
            &mut checkpoint.class_coverage,
            &checkpoint.completed_tasks,
            &checkpoint.findings_so_far,
        );
    }
    let pending_count = checkpoint
        .pending_tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Pending)
        .count();
    let kind = if d.is_class {
        "hunt specialist"
    } else {
        "stack specialist"
    };
    aggregate.checkpoints.push(CheckpointEvent {
        message: format!(
            "security_engineer: {kind} `{}` complete ({} completed, {} pending)",
            d.label,
            checkpoint.completed_tasks.len(),
            pending_count
        ),
        progress: Some(0.35),
    });
    Ok(())
}

/// All still-pending tasks sharing `class_id` — the work for one specialist.
pub(super) fn pending_tasks_for_class(
    checkpoint: &SecurityCheckpoint,
    class_id: &str,
) -> Vec<SecurityTask> {
    checkpoint
        .pending_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Pending && t.attack_class == class_id)
        .cloned()
        .collect()
}

pub(super) fn complete_tasks(
    checkpoint: &mut SecurityCheckpoint,
    completed_ids: &BTreeSet<String>,
) {
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

pub(super) fn normalize_task_ids(tasks: &mut [SecurityTask], prefix: &str) {
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.id.trim().is_empty() {
            task.id = format!("{prefix}-{:03}", idx + 1);
        }
    }
}

pub(super) async fn run_validate_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    if checkpoint.findings_so_far.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("../prompts/security_engineer_validate.md");
    let (raw, stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Validate,
        prompt,
        checkpoint,
        VALIDATE_MAX_ITERATIONS,
    )
    .await?;
    let validate = parse_validate_output(&raw, &checkpoint.findings_so_far)?;
    checkpoint
        .validation_decisions_so_far
        .extend(validate.decisions);
    Ok(Some(stage_out))
}

pub(super) fn run_gapfill_stage(checkpoint: &mut SecurityCheckpoint) {
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
            attack_class: canonical_vulnerability_class(&gap.area)
                .unwrap_or("resource_exhaustion_dos")
                .into(),
            scope_hint: gap.area.clone(),
            status: TaskStatus::Pending,
            rationale: gap.reason.clone(),
        });
    }
    update_class_coverage_task_ids(&mut checkpoint.class_coverage, &additions);
    checkpoint.gapfill_tasks.extend(additions.clone());
    checkpoint.pending_tasks.extend(additions);
}

pub(crate) fn run_dedupe_stage(checkpoint: &mut SecurityCheckpoint) {
    let findings = super::report::reportable_confirmed_findings(checkpoint)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    checkpoint.dedupe_groups_so_far = super::report::dedupe_findings(&findings);
}

pub(super) async fn run_trace_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let confirmed: Vec<_> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Confirmed)
        .collect();
    if confirmed.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("../prompts/security_engineer_trace.md");
    let (raw, stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Trace,
        prompt,
        checkpoint,
        TRACE_MAX_ITERATIONS,
    )
    .await?;
    match parse_stage_json::<TraceStageOutput>(&raw) {
        Ok(traces) => {
            checkpoint.trace_results_so_far.extend(traces.traces);
        }
        Err(err) => {
            checkpoint.coverage_gaps.push(CoverageGap {
                area: "Trace stage".into(),
                reason: format!(
                    "Trace stage output was not parseable JSON; continuing with existing reachability evidence: {err}"
                ),
                risk: "unknown".into(),
            });
            checkpoint.report_validation_state = ReportValidationState {
                status: "trace_unparsed".into(),
                errors: vec![err],
            };
        }
    }
    Ok(Some(stage_out))
}

pub(super) fn run_feedback_stage(checkpoint: &mut SecurityCheckpoint) {
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
                attack_class: canonical_vulnerability_class(&finding.vulnerability_class)
                    .unwrap_or("injection_unsafe_execution")
                    .into(),
                scope_hint: path.clone(),
                status: TaskStatus::Pending,
                rationale: "reachable shared-component finding; inspect consumer path".into(),
            });
            if let Some(task) = checkpoint.pending_tasks.last() {
                update_class_coverage_task_ids(
                    &mut checkpoint.class_coverage,
                    std::slice::from_ref(task),
                );
            }
            next += 1;
        }
    }
}

pub(super) async fn run_report_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("../prompts/security_engineer_report.md");
    let checkpoint_for_prompt = report_checkpoint_for_prompt(checkpoint);
    let (raw, mut stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Report,
        prompt,
        &checkpoint_for_prompt,
        10,
    )
    .await?;
    let (parsed, validation_state) = match parse_report_output(&raw) {
        Ok(report) => (
            report,
            ReportValidationState {
                status: "valid".into(),
                errors: Vec::new(),
            },
        ),
        Err(first_err) => {
            checkpoint.report_validation_state = ReportValidationState {
                status: "repairing".into(),
                errors: vec![first_err.clone()],
            };
            let repair_prompt = include_str!("../prompts/security_engineer_report_repair.md");
            let mut repair_checkpoint = checkpoint_for_prompt.clone();
            repair_checkpoint.report_validation_state = checkpoint.report_validation_state.clone();
            let (repair_raw, repair_out) = spawn_stage(
                rt,
                SecurityHarnessStage::Report,
                repair_prompt,
                &repair_checkpoint,
                6,
            )
            .await?;
            merge_stage_tool_output(&mut stage_out, repair_out);
            resolve_repaired_or_fallback_report(checkpoint, &first_err, &repair_raw)?
        }
    };
    checkpoint.report_validation_state = validation_state;
    checkpoint.report_draft = Some(parsed);
    Ok(Some(stage_out))
}

pub(crate) fn resolve_repaired_or_fallback_report(
    checkpoint: &SecurityCheckpoint,
    first_err: &str,
    repair_raw: &str,
) -> std::result::Result<(SecurityHarnessReport, ReportValidationState), String> {
    match parse_report_output(repair_raw) {
        Ok(report) => Ok((
            report,
            ReportValidationState {
                status: "valid".into(),
                errors: Vec::new(),
            },
        )),
        Err(second_err) => {
            let fallback = report_from_checkpoint(checkpoint);
            let fallback_value = serde_json::to_value(&fallback)
                .map_err(|e| format!("serialize deterministic report fallback: {e}"))?;
            match super::parse::validate_report_json(&fallback_value) {
                Ok(report) => Ok((
                    report,
                    ReportValidationState {
                        status: "deterministic_fallback".into(),
                        errors: vec![first_err.to_string(), second_err],
                    },
                )),
                Err(fallback_err) => Err(format!(
                    "report schema validation failed after repair: {first_err}; {second_err}; deterministic fallback failed: {fallback_err}"
                )),
            }
        }
    }
}

pub(super) fn stage_completed(
    checkpoint: &SecurityCheckpoint,
    stage: SecurityHarnessStage,
) -> bool {
    checkpoint
        .stage_history
        .iter()
        .any(|entry| entry.stage == stage && entry.status == "completed")
}

pub(super) fn stage_summary(
    checkpoint: &SecurityCheckpoint,
    stage: SecurityHarnessStage,
) -> String {
    match stage {
        SecurityHarnessStage::Recon => format!(
            "{} pending hunt tasks; {} applicable classes; {} gaps",
            checkpoint.pending_tasks.len(),
            checkpoint
                .class_coverage
                .iter()
                .filter(|class| class.applicable)
                .count(),
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

pub(super) fn progress_for(stage: SecurityHarnessStage) -> Option<f32> {
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
