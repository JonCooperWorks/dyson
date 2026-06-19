//! Stage runners for the security_engineer harness.
//!
//! One async (or sync, for the bookkeeping-only stages) function per stage:
//!
//!   Recon -> Hunt -> Validate -> Gapfill -> Dedupe -> Trace -> Feedback -> Report
//!
//! Each stage reads/writes the shared SecurityCheckpoint; the harness loop in
//! mod.rs persists the checkpoint between stages.  Stage runners return
//! `Option<ToolOutput>` so bookkeeping-only stages (Gapfill/Dedupe/Feedback)
//! can avoid emitting empty stage outputs.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::tool::{CheckpointEvent, ToolOutput};

use super::checkpoint::{CheckpointStore, unix_seconds};
use super::parse::{
    parse_report_output, parse_stage_json, parse_validate_output_shape, validate_decisions_semantic,
};
use super::report::{report_checkpoint_for_prompt, report_from_checkpoint};
use super::runtime::{SecurityHarnessRuntime, merge_stage_tool_output, spawn_stage};
use super::stack::{StackSpecialist, prune_inapplicable_class_tasks, stack_specialists};
use super::taxonomy::{
    build_class_coverage, canonical_vulnerability_class, canonicalize_findings, canonicalize_tasks,
    class_specialist_briefing, ensure_taxonomy_hunt_tasks, mark_hunted_class_coverage,
    update_class_coverage_task_ids,
};
use super::types::{
    CoverageGap, HuntStageOutput, ReconStageOutput, ReportValidationState, SecurityCheckpoint,
    SecurityHarnessReport, SecurityHarnessStage, SecurityTask, SeverityRollup, TaskStatus,
    TraceStageOutput, ValidationDecisionKind,
};
use super::{
    HUNT_MAX_ITERATIONS, HUNT_SPECIALIST_BACKSTOP, RECON_MAX_ITERATIONS, TRACE_MAX_ITERATIONS,
    VALIDATE_MAX_ITERATIONS,
};

const HUNT_CONCURRENCY: usize = 6;

/// Hard wall-clock backstop for a single Hunt specialist child.
///
/// This is a COARSE runaway guard, not the primary anti-hang mechanism — that
/// is the transport read timeout (`http::READ_TIMEOUT`), which makes any
/// stalled LLM stream error within ~2 min so the child's agent loop retries or
/// returns instead of blocking forever.  Combined with the per-child iteration
/// cap (`HUNT_MAX_ITERATIONS`), a healthy specialist always terminates on its
/// own; this budget only needs to catch a child wedged in a way neither covers
/// (e.g. a hung non-HTTP tool).  A specialist that blows the budget is folded
/// as a coverage gap (see [`fold_hunt_degraded`]), never a fatal error.
///
/// SIZED GENEROUSLY, and deliberately tied to the iteration cap.  A flat 7-min
/// value cut EVERY one of the 24 class specialists on a large repo (the vLLM
/// review — each specialist legitimately needs many ast_query/taint_trace
/// turns over a big tree, blew 7 min, and degraded its whole class to a
/// coverage gap; the run "succeeded" with near-zero coverage).  Cutting a
/// progressing specialist is far costlier than letting a rare wedged child run
/// longer, so budget a full read-timeout per iteration: a full-depth specialist
/// finishes within ~`HUNT_MAX_ITERATIONS * READ_TIMEOUT` and never trips this.
/// Small targets are unaffected (their specialists finish in 1–7 min).
/// See docs/qa/2026-06-09-hunt-child-timeout-degradation.md.
const HUNT_CHILD_TIMEOUT: Duration = Duration::from_secs(HUNT_MAX_ITERATIONS as u64 * 120);

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
        fold_hunt_wave(
            checkpoint,
            &mut aggregate,
            dispatch_hunts(rt, dispatches).await,
        );
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        store.save(checkpoint).await?;
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
        fold_hunt_wave(
            checkpoint,
            &mut aggregate,
            dispatch_hunts(rt, dispatches).await,
        );
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        store.save(checkpoint).await?;
    }

    if ran_batch {
        // Rollup findings-by-severity for the SecurityHarnessPanel
        // counter.  Matches the regex in panels.jsx's parseHarnessState:
        //   `security_engineer: findings critical=N high=N medium=N low=N`
        let r = SeverityRollup::from_findings(&checkpoint.findings_so_far);
        aggregate.checkpoints.push(CheckpointEvent {
            message: format!(
                "security_engineer: findings critical={} high={} medium={} low={}",
                r.critical, r.high, r.medium, r.low
            ),
            progress: Some(0.45),
        });
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
        // Bound every child by HUNT_CHILD_TIMEOUT.  Dropping the future on
        // elapse cancels the in-flight child (and its reqwest stream) at the
        // next await point, so a wedged specialist frees its concurrency slot
        // instead of stalling the whole `buffer_unordered` wave forever.  The
        // timeout maps to the same `Err` channel a real child error uses, so
        // `fold_hunt_wave` folds it as a coverage gap.
        let fut = spawn_stage(
            rt,
            SecurityHarnessStage::Hunt,
            &d.stage_prompt,
            &d.checkpoint,
            HUNT_MAX_ITERATIONS,
        );
        let res = match tokio::time::timeout(HUNT_CHILD_TIMEOUT, fut).await {
            Ok(res) => res,
            Err(_) => Err(hunt_child_timeout_error(&d.label)),
        };
        (d, res)
    }))
    .buffer_unordered(HUNT_CONCURRENCY)
    .collect()
    .await
}

/// The error string a Hunt specialist gets when it blows [`HUNT_CHILD_TIMEOUT`].
/// Folded as a coverage gap, never fatal — factored out so the message format
/// is unit-testable without spawning a real child.
pub(super) fn hunt_child_timeout_error(label: &str) -> String {
    format!(
        "hunt specialist '{label}' exceeded its {}s wall-clock budget",
        HUNT_CHILD_TIMEOUT.as_secs()
    )
}

/// Fold a completed wave's results into the checkpoint.
///
/// Every specialist is folded independently and NON-FATALLY: a child that
/// errored or timed out is folded as a coverage gap (see
/// [`fold_hunt_degraded`]) exactly like a child that returned unparseable
/// prose — it had its turn, produced nothing usable, its batch is marked
/// complete so the class isn't re-dispatched forever, and the harness moves
/// on.  One flaky upstream call must never deadlock or fail the whole
/// multi-stage run; the report notes the reduced coverage instead.
pub(super) fn fold_hunt_wave(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    results: Vec<HuntOutcome>,
) {
    for (d, res) in results {
        match res {
            Ok((raw, stage_out)) => {
                fold_hunt_result(checkpoint, aggregate, &d, raw, stage_out);
            }
            Err(e) => {
                fold_hunt_degraded(checkpoint, aggregate, &d, &e);
            }
        }
    }
}

/// Fold a specialist that did not complete — a transport error, a timeout, or
/// a spawn failure.  Mirrors the prose-fallback path in [`fold_hunt_result`]:
/// mark the specialist's batch complete (so a still-pending class is not
/// re-dispatched into an infinite loop), record a coverage gap so the report
/// surfaces the blind spot honestly, and emit a panel event.  Then the wave
/// loop continues — the run completes with whatever the other specialists
/// found.
pub(super) fn fold_hunt_degraded(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    d: &HuntDispatch,
    err: &str,
) {
    if d.is_class {
        let ids: BTreeSet<String> = d.batch_ids.iter().cloned().collect();
        complete_tasks(checkpoint, &ids);
    }
    checkpoint.coverage_gaps.push(CoverageGap {
        area: d.label.clone(),
        reason: format!("hunt specialist did not complete: {err}"),
        risk: "unknown".into(),
    });
    tracing::warn!(
        specialist = %d.label,
        error = %err,
        "hunt specialist did not complete; folding as coverage gap and continuing"
    );
    // Class-scoped panel signal, parallel to the `hunt: class X hunted/cleared`
    // lines.  Stack specialists aren't class-bound, so (like the success path)
    // they don't emit a per-class line — only the coverage gap above.
    if d.is_class {
        aggregate.checkpoints.push(CheckpointEvent {
            message: format!(
                "security_engineer: hunt: class {} degraded (specialist did not complete)",
                d.label
            ),
            progress: Some(0.35),
        });
    }
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
) {
    merge_stage_tool_output(aggregate, stage_out);
    // Loose at the shape boundary: a specialist that returns prose instead
    // of JSON drops ITS findings, not the whole wave.  The class still gets
    // marked complete below (the specialist had its turn), and the next
    // stage proceeds with what other specialists returned.  Mirrors recon's
    // non-fatal pattern — every previous catastrophic failure we debugged
    // was a stage-boundary parse killing the run.
    let hunt: HuntStageOutput = parse_stage_json(&raw).unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            specialist = %d.label,
            "hunt specialist output did not parse as JSON; dropping its findings, \
             marking its batch complete"
        );
        HuntStageOutput::default()
    });
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
    // Parseable per-class outcome line for the SecurityHarnessPanel.
    // Format matches the regex in panels.jsx's parseHarnessState:
    //   `hunt: class <class_id> hunted (N findings)`
    //   `hunt: class <class_id> cleared`
    // Stack specialists are NOT class-scoped, so they don't emit here.
    if d.is_class {
        // Count findings from this specialist's run.  fold_hunt_result has
        // already pushed the findings into checkpoint.findings_so_far above,
        // but we don't have a quick way to attribute "which finding came
        // from which class" without scanning vulnerability_class.  Count
        // findings whose vulnerability_class matches this specialist's
        // label (canonicalized to the class id).
        let class_id = &d.label;
        let class_finding_count = checkpoint
            .findings_so_far
            .iter()
            .filter(|f| f.vulnerability_class == *class_id)
            .count();
        let status_word = if class_finding_count > 0 {
            "hunted"
        } else {
            "cleared"
        };
        let suffix = if class_finding_count > 0 {
            format!(" ({class_finding_count} findings)")
        } else {
            String::new()
        };
        aggregate.checkpoints.push(CheckpointEvent {
            message: format!("security_engineer: hunt: class {class_id} {status_word}{suffix}"),
            progress: Some(0.35),
        });
    }
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
    // Shape loose: if the validator returned prose or a wrong-shaped JSON,
    // we'd rather carry forward zero new decisions than kill the run.
    // Semantic strict: if the shape is fine but the validator hallucinated
    // a finding_id, confirmed a finding missing required evidence, or
    // confirmed a no-vulnerability note, fail loudly — that's a quality-
    // floor violation, and letting it through would put junk in the report.
    let validate = match parse_validate_output_shape(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "validate stage output did not parse as JSON; carrying \
                 existing decisions, no new decisions added"
            );
            super::types::ValidateStageOutput::default()
        }
    };
    validate_decisions_semantic(&validate, &checkpoint.findings_so_far)?;
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

#[cfg(test)]
mod tests {
    use super::super::types::{ModelMetadata, StageHistoryEntry, TargetRef};
    use super::*;

    fn cp() -> SecurityCheckpoint {
        SecurityCheckpoint::new(
            "run".into(),
            TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        )
    }

    fn pending(id: &str, class: &str) -> SecurityTask {
        SecurityTask {
            id: id.into(),
            attack_class: class.into(),
            scope_hint: "scope".into(),
            status: TaskStatus::Pending,
            rationale: String::new(),
        }
    }

    #[test]
    fn distinct_pending_classes_dedupes_and_preserves_first_seen_order() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        c.pending_tasks
            .push(pending("t2", "injection_unsafe_execution"));
        c.pending_tasks.push(pending("t3", "auth_authorization"));
        c.pending_tasks.push(pending("t4", "ssrf_outbound_network"));
        assert_eq!(
            distinct_pending_classes(&c),
            vec![
                "auth_authorization".to_string(),
                "injection_unsafe_execution".to_string(),
                "ssrf_outbound_network".to_string(),
            ],
            "distinct classes must dedupe and preserve first-seen order"
        );
    }

    #[test]
    fn distinct_pending_classes_skips_completed_tasks() {
        let mut c = cp();
        let mut done = pending("t-done", "auth_authorization");
        done.status = TaskStatus::Completed;
        c.pending_tasks.push(done);
        c.pending_tasks
            .push(pending("t-pending", "ssrf_outbound_network"));
        assert_eq!(
            distinct_pending_classes(&c),
            vec!["ssrf_outbound_network".to_string()],
            "completed tasks should be skipped"
        );
    }

    #[test]
    fn pending_tasks_for_class_returns_pending_of_that_class() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        c.pending_tasks
            .push(pending("t2", "injection_unsafe_execution"));
        c.pending_tasks.push(pending("t3", "auth_authorization"));
        let pending_auth = pending_tasks_for_class(&c, "auth_authorization");
        let ids: Vec<&str> = pending_auth.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t1", "t3"],
            "should return only pending auth_authorization tasks in order"
        );
    }

    #[test]
    fn pending_tasks_for_class_returns_empty_for_missing_class() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        assert!(
            pending_tasks_for_class(&c, "missing_class").is_empty(),
            "missing class should return empty list"
        );
    }

    #[test]
    fn complete_tasks_moves_matching_ids_and_flips_status() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        c.pending_tasks
            .push(pending("t2", "injection_unsafe_execution"));
        let ids: BTreeSet<String> = ["t1".to_string()].into_iter().collect();
        complete_tasks(&mut c, &ids);
        assert_eq!(
            c.pending_tasks.len(),
            1,
            "t1 should have been removed from pending"
        );
        assert_eq!(
            c.pending_tasks[0].id, "t2",
            "remaining pending task should be t2"
        );
        assert_eq!(
            c.completed_tasks.len(),
            1,
            "t1 should have been added to completed"
        );
        assert_eq!(c.completed_tasks[0].id, "t1");
        assert_eq!(
            c.completed_tasks[0].status,
            TaskStatus::Completed,
            "completed task status must be flipped to Completed"
        );
    }

    #[test]
    fn complete_tasks_with_empty_set_is_a_no_op() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        let ids: BTreeSet<String> = BTreeSet::new();
        complete_tasks(&mut c, &ids);
        assert_eq!(c.pending_tasks.len(), 1);
        assert!(c.completed_tasks.is_empty());
    }

    #[test]
    fn complete_tasks_with_unknown_ids_is_a_no_op() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        let ids: BTreeSet<String> = ["unknown".to_string()].into_iter().collect();
        complete_tasks(&mut c, &ids);
        assert_eq!(c.pending_tasks.len(), 1);
        assert!(c.completed_tasks.is_empty());
    }

    #[test]
    fn normalize_task_ids_backfills_empty_ids_with_prefix_and_index() {
        let mut tasks = vec![
            SecurityTask {
                id: String::new(),
                ..Default::default()
            },
            SecurityTask {
                id: String::new(),
                ..Default::default()
            },
        ];
        normalize_task_ids(&mut tasks, "gap");
        assert_eq!(tasks[0].id, "gap-001");
        assert_eq!(tasks[1].id, "gap-002");
    }

    #[test]
    fn normalize_task_ids_leaves_non_empty_ids_unchanged() {
        let mut tasks = vec![
            SecurityTask {
                id: "given".into(),
                ..Default::default()
            },
            SecurityTask {
                id: String::new(),
                ..Default::default()
            },
        ];
        normalize_task_ids(&mut tasks, "gap");
        assert_eq!(tasks[0].id, "given", "existing id should not be changed");
        // The index-based backfill still uses the original index — even
        // when an earlier slot was non-empty.
        assert_eq!(
            tasks[1].id, "gap-002",
            "backfill uses index in the input slice"
        );
    }

    #[test]
    fn stage_completed_flips_when_stage_history_records_completion() {
        let mut c = cp();
        assert!(
            !stage_completed(&c, SecurityHarnessStage::Recon),
            "fresh checkpoint must not report any stage completed"
        );
        c.stage_history.push(StageHistoryEntry {
            stage: SecurityHarnessStage::Recon,
            status: "completed".into(),
            started_at: 0,
            finished_at: 0,
            summary: String::new(),
            model: String::new(),
        });
        assert!(
            stage_completed(&c, SecurityHarnessStage::Recon),
            "checkpoint must report Recon completed after history entry"
        );
        assert!(
            !stage_completed(&c, SecurityHarnessStage::Hunt),
            "Hunt should still be incomplete"
        );
    }

    #[test]
    fn stage_summary_returns_non_empty_for_every_stage() {
        let c = cp();
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
            let summary = stage_summary(&c, stage);
            // Report defers to checkpoint.report_validation_state.status,
            // which defaults to "not_started" on a fresh checkpoint, so
            // every stage summary should be non-empty.
            assert!(
                !summary.is_empty(),
                "stage_summary({stage}) returned empty string"
            );
        }
    }

    #[test]
    fn progress_for_returns_value_in_unit_interval_for_every_stage() {
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
            let progress = progress_for(stage).expect("progress should always be Some");
            assert!(
                (0.0..=1.0).contains(&progress),
                "progress_for({stage})={progress} should be in [0,1]"
            );
        }
    }

    // ---- Loose-at-shape regressions for hunt + validate -------------------

    #[test]
    fn fold_hunt_result_with_prose_drops_findings_marks_class_complete() {
        // The specialist returned prose instead of JSON.  Before: that
        // bubbled up as Err and killed the whole hunt wave.  Now: the
        // specialist's batch_ids get marked complete (it had its turn,
        // produced nothing) and the harness moves on.  This is the single
        // most important loosening — one bad specialist no longer takes
        // down the run.
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        c.pending_tasks.push(pending("t2", "ssrf_outbound_network"));
        let dispatch = HuntDispatch {
            label: "auth_authorization".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t1".into()],
            is_class: true,
        };
        let mut aggregate = ToolOutput::success("");
        let stage_out = ToolOutput::success("");
        let prose = String::from("I will now write the report.\nNo JSON forthcoming.");
        fold_hunt_result(&mut c, &mut aggregate, &dispatch, prose, stage_out);
        assert_eq!(c.findings_so_far.len(), 0);
        // t1 should be in completed_tasks (its specialist had its turn).
        // t2 (different class) should still be pending.
        assert!(c.completed_tasks.iter().any(|t| t.id == "t1"));
        assert!(c.pending_tasks.iter().any(|t| t.id == "t2"));
    }

    #[test]
    fn fold_hunt_result_with_valid_json_still_works() {
        // Make sure the loose path didn't break the success path.
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        let dispatch = HuntDispatch {
            label: "auth_authorization".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t1".into()],
            is_class: true,
        };
        let mut aggregate = ToolOutput::success("");
        let stage_out = ToolOutput::success("");
        let raw = String::from(
            r#"{"completed_task_ids":["t1"],"findings":[{"id":"f-1","title":"hi","vulnerability_class":"auth_authorization"}],"gaps":[],"follow_up_tasks":[]}"#,
        );
        fold_hunt_result(&mut c, &mut aggregate, &dispatch, raw, stage_out);
        assert_eq!(c.findings_so_far.len(), 1);
        assert!(c.completed_tasks.iter().any(|t| t.id == "t1"));
    }

    // ---- Phase 2 backend signals -----------------------------------------

    #[test]
    fn fold_hunt_result_emits_class_hunted_event_with_finding_count() {
        // When a class specialist finds N findings, the CheckpointEvent
        // stream gets a parseable `hunt: class X hunted (N findings)`
        // line — the SecurityHarnessPanel reads it to populate the class
        // coverage grid.
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        let dispatch = HuntDispatch {
            label: "auth_authorization".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t1".into()],
            is_class: true,
        };
        let mut aggregate = ToolOutput::success("");
        let raw = String::from(
            r#"{"completed_task_ids":["t1"],"findings":[
                {"id":"f-1","title":"a","vulnerability_class":"auth_authorization"},
                {"id":"f-2","title":"b","vulnerability_class":"auth_authorization"}
            ],"gaps":[],"follow_up_tasks":[]}"#,
        );
        fold_hunt_result(
            &mut c,
            &mut aggregate,
            &dispatch,
            raw,
            ToolOutput::success(""),
        );
        let class_line = aggregate
            .checkpoints
            .iter()
            .find(|cp| cp.message.contains("hunt: class auth_authorization"))
            .expect("per-class checkpoint event should be emitted");
        assert!(
            class_line.message.contains("hunted"),
            "should mark class hunted when findings > 0: {}",
            class_line.message
        );
        assert!(
            class_line.message.contains("(2 findings)"),
            "should include the finding count: {}",
            class_line.message
        );
    }

    #[test]
    fn fold_hunt_result_emits_class_cleared_event_when_no_findings() {
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "session_oauth_csrf"));
        let dispatch = HuntDispatch {
            label: "session_oauth_csrf".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t1".into()],
            is_class: true,
        };
        let mut aggregate = ToolOutput::success("");
        let raw = String::from(
            r#"{"completed_task_ids":["t1"],"findings":[],"gaps":[],"follow_up_tasks":[]}"#,
        );
        fold_hunt_result(
            &mut c,
            &mut aggregate,
            &dispatch,
            raw,
            ToolOutput::success(""),
        );
        let class_line = aggregate
            .checkpoints
            .iter()
            .find(|cp| cp.message.contains("hunt: class session_oauth_csrf"))
            .expect("per-class checkpoint event should be emitted");
        assert!(
            class_line.message.contains("cleared"),
            "no findings → cleared: {}",
            class_line.message
        );
        assert!(
            !class_line.message.contains("findings)"),
            "cleared shouldn't carry a (N findings) tail: {}",
            class_line.message
        );
    }

    #[test]
    fn fold_hunt_result_does_not_emit_class_event_for_stack_specialists() {
        // Stack specialists hunt synthetic tasks not bound to a single
        // taxonomy class.  They should NOT emit a class-coverage line.
        let mut c = cp();
        let dispatch = HuntDispatch {
            label: "express-framework".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: Vec::new(),
            is_class: false,
        };
        let mut aggregate = ToolOutput::success("");
        let raw = String::from(
            r#"{"completed_task_ids":[],"findings":[],"gaps":[],"follow_up_tasks":[]}"#,
        );
        fold_hunt_result(
            &mut c,
            &mut aggregate,
            &dispatch,
            raw,
            ToolOutput::success(""),
        );
        assert!(
            !aggregate
                .checkpoints
                .iter()
                .any(|cp| cp.message.contains("hunt: class")),
            "stack specialists shouldn't emit per-class outcome lines"
        );
    }

    // ---- Resilience: a stalled/erroring specialist must not be fatal --------

    #[test]
    fn fold_hunt_degraded_class_marks_batch_complete_and_records_gap() {
        // A class specialist that errored (transport stall, timeout, spawn
        // failure) must: (1) mark its batch complete so the still-pending
        // class is never re-dispatched into an infinite wave loop, (2) leave
        // a coverage gap so the report surfaces the blind spot, and (3) emit a
        // `degraded` panel line — all WITHOUT adding findings or panicking.
        let mut c = cp();
        c.pending_tasks.push(pending("t1", "auth_authorization"));
        c.pending_tasks.push(pending("t2", "ssrf_outbound_network"));
        let dispatch = HuntDispatch {
            label: "auth_authorization".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t1".into()],
            is_class: true,
        };
        let mut aggregate = ToolOutput::success("");
        fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "upstream stalled");
        assert_eq!(c.findings_so_far.len(), 0, "degraded fold adds no findings");
        assert!(
            c.completed_tasks.iter().any(|t| t.id == "t1"),
            "the erroring specialist's batch must be marked complete to break the wave loop"
        );
        assert!(
            c.pending_tasks.iter().any(|t| t.id == "t2"),
            "an unrelated pending class must stay pending"
        );
        assert!(
            c.coverage_gaps
                .iter()
                .any(|g| g.area == "auth_authorization" && g.reason.contains("upstream stalled")),
            "a coverage gap must record the failure: {:?}",
            c.coverage_gaps
        );
        let line = aggregate
            .checkpoints
            .iter()
            .find(|cp| cp.message.contains("hunt: class auth_authorization"))
            .expect("degraded specialist should emit a per-class panel line");
        assert!(
            line.message.contains("degraded"),
            "per-class line should mark the class degraded: {}",
            line.message
        );
    }

    #[test]
    fn fold_hunt_degraded_stack_specialist_records_gap_without_class_line() {
        // Stack specialists aren't class-bound, so a failed one records a
        // coverage gap but emits no per-class panel line (parity with the
        // success path).
        let mut c = cp();
        let dispatch = HuntDispatch {
            label: "flask".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: Vec::new(),
            is_class: false,
        };
        let mut aggregate = ToolOutput::success("");
        fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "timed out");
        assert!(
            c.coverage_gaps.iter().any(|g| g.area == "flask"),
            "stack specialist failure should still record a coverage gap"
        );
        assert!(
            !aggregate
                .checkpoints
                .iter()
                .any(|cp| cp.message.contains("hunt: class")),
            "stack specialists must not emit a per-class line even when degraded"
        );
    }

    #[test]
    fn fold_hunt_wave_is_nonfatal_and_folds_success_and_error_together() {
        // The whole point of the fix: a wave with one good specialist and one
        // failed specialist folds BOTH — the good one's findings land, the
        // failed one becomes a coverage gap + completed batch — and the call
        // returns `()` (no fatal error bubbles up to kill the run).
        let mut c = cp();
        c.pending_tasks.push(pending("t-ok", "auth_authorization"));
        c.pending_tasks
            .push(pending("t-bad", "ssrf_outbound_network"));
        let ok = HuntDispatch {
            label: "auth_authorization".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t-ok".into()],
            is_class: true,
        };
        let bad = HuntDispatch {
            label: "ssrf_outbound_network".into(),
            stage_prompt: String::new(),
            checkpoint: c.clone(),
            batch_ids: vec!["t-bad".into()],
            is_class: true,
        };
        let ok_raw = String::from(
            r#"{"completed_task_ids":["t-ok"],"findings":[{"id":"f-1","title":"x","vulnerability_class":"auth_authorization"}],"gaps":[],"follow_up_tasks":[]}"#,
        );
        let results: Vec<HuntOutcome> = vec![
            (ok, Ok((ok_raw, ToolOutput::success("")))),
            (bad, Err("HTTP error: error sending request".to_string())),
        ];
        let mut aggregate = ToolOutput::success("");
        // Returns (), never panics: non-fatal by construction.
        fold_hunt_wave(&mut c, &mut aggregate, results);
        assert_eq!(
            c.findings_so_far.len(),
            1,
            "the successful specialist's finding must be folded"
        );
        assert!(
            c.completed_tasks.iter().any(|t| t.id == "t-ok")
                && c.completed_tasks.iter().any(|t| t.id == "t-bad"),
            "both the successful and the failed specialist's batches must be marked complete"
        );
        assert!(
            c.pending_tasks
                .iter()
                .all(|t| t.status != TaskStatus::Pending),
            "no class should remain pending → the wave loop terminates, no deadlock"
        );
        assert!(
            c.coverage_gaps
                .iter()
                .any(|g| g.area == "ssrf_outbound_network"),
            "the failed specialist must leave a coverage gap"
        );
    }

    #[test]
    fn hunt_child_timeout_error_names_label_and_budget() {
        let msg = hunt_child_timeout_error("injection_unsafe_execution");
        assert!(msg.contains("injection_unsafe_execution"), "msg={msg}");
        assert!(
            msg.contains(&HUNT_CHILD_TIMEOUT.as_secs().to_string()),
            "timeout message should name the budget seconds: {msg}"
        );
    }

    #[test]
    fn hunt_child_timeout_is_generous_relative_to_iteration_cap() {
        // Regression for the vLLM degradation bug: a too-tight per-child budget
        // (a flat 7 min) cut EVERY one of the 24 class specialists on a large
        // repo, silently degrading every vulnerability class to a coverage gap
        // while the run still reported "success".  The wall-clock backstop must
        // never be tighter than the legitimate worst-case child runtime, which
        // is bounded by the iteration cap (and the transport read timeout per
        // call), not by the clock.  Require at least ~90s of headroom per
        // iteration so a full-depth specialist always finishes inside the bound.
        // Setting HUNT_CHILD_TIMEOUT back to 420s fails this.
        assert!(
            HUNT_CHILD_TIMEOUT.as_secs() >= HUNT_MAX_ITERATIONS as u64 * 90,
            "HUNT_CHILD_TIMEOUT ({}s) must allow a full-depth specialist \
             (HUNT_MAX_ITERATIONS={}) to finish; a tighter budget silently \
             degrades whole vulnerability classes to coverage gaps on large repos",
            HUNT_CHILD_TIMEOUT.as_secs(),
            HUNT_MAX_ITERATIONS,
        );
    }
}
