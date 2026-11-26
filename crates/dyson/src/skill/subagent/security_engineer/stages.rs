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
use super::report::{
    reconcile_report_with_checkpoint, report_checkpoint_for_prompt, report_from_checkpoint,
};
use super::runtime::{SecurityHarnessRuntime, merge_stage_tool_output, spawn_stage};
use super::stack::{StackSpecialist, prune_inapplicable_class_tasks, stack_specialists};
use super::taxonomy::{
    build_class_coverage, canonical_vulnerability_class, canonicalize_findings, canonicalize_tasks,
    class_specialist_briefing, ensure_taxonomy_hunt_tasks, mark_hunted_class_coverage,
    update_class_coverage_task_ids,
};
use super::types::{
    CoverageGap, HuntStageOutput, JudgmentStageOutput, ReconStageOutput, ReportValidationState,
    SecurityCheckpoint, SecurityFinding, SecurityHarnessReport, SecurityHarnessStage, SecurityTask,
    SeverityRollup, TaskStatus, TraceStageOutput, ValidateStageOutput, ValidationDecisionKind,
};
use super::{
    HUNT_MAX_ITERATIONS, HUNT_SPECIALIST_BACKSTOP, JUDGMENT_MAX_ITERATIONS, RECON_MAX_ITERATIONS,
    TRACE_MAX_ITERATIONS, VALIDATE_MAX_ITERATIONS,
};

/// Default in-wave concurrency for specialist/shard children. Overridable via
/// `DYSON_SEC_HUNT_CONCURRENCY` (clamped to 1..=32). Bounds how many children
/// the `buffer_unordered` waves poll at once; real LLM concurrency is further
/// bounded by the client's own limiter underneath.
mod hunt;

pub(super) use self::hunt::*;

const DEFAULT_STAGE_CONCURRENCY: usize = 6;

fn stage_concurrency() -> usize {
    std::env::var("DYSON_SEC_HUNT_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 32))
        .unwrap_or(DEFAULT_STAGE_CONCURRENCY)
}

/// Findings per Validate/Trace shard. A stage with this many findings or fewer
/// runs as a single child (today's behaviour, preserving the validator's
/// holistic view of a small finding set); larger sets fan out into concurrent
/// shards keyed per-finding, where cross-finding reasoning is irrelevant
/// because dedupe is a separate deterministic stage.
const STAGE_SHARD_SIZE: usize = 8;

/// Whether the opt-in shallow-hunt requeue is enabled. Off by default; any
/// truthy `DYSON_SEC_REQUEUE_SHALLOW` value (set, non-empty, not `0`/`false`)
/// turns it on. A degraded class specialist then gets one extra attempt before
/// its blind spot is recorded as a coverage gap.
fn requeue_shallow_enabled() -> bool {
    std::env::var("DYSON_SEC_REQUEUE_SHALLOW")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

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

/// Split a checkpoint into per-shard copies, each carrying a chunk of `findings`
/// in `findings_so_far`. Returns a single shard (the whole set) when
/// `findings.len() <= STAGE_SHARD_SIZE`, preserving today's single-child
/// behaviour for small reviews; larger sets fan out.
fn shard_by_findings(
    checkpoint: &SecurityCheckpoint,
    findings: &[SecurityFinding],
) -> Vec<SecurityCheckpoint> {
    let size = if findings.len() > STAGE_SHARD_SIZE {
        STAGE_SHARD_SIZE
    } else {
        findings.len().max(1)
    };
    findings
        .chunks(size)
        .map(|chunk| {
            let mut cp = checkpoint.clone();
            cp.findings_so_far = chunk.to_vec();
            cp
        })
        .collect()
}

/// Run each shard checkpoint through `stage` concurrently (bounded by
/// [`stage_concurrency`]). Per-finding stages (Validate/Trace) have no
/// cross-shard dependency — each shard's decisions/traces are keyed by
/// `finding_id` — so the caller concatenates the results.
async fn dispatch_checkpoint_shards(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    shards: Vec<SecurityCheckpoint>,
    max_iterations: usize,
) -> Vec<std::result::Result<(String, ToolOutput), String>> {
    use futures_util::stream::StreamExt;
    futures_util::stream::iter(
        shards
            .into_iter()
            .map(|cp| async move { spawn_stage(rt, stage, prompt, &cp, max_iterations).await }),
    )
    .buffer_unordered(stage_concurrency())
    .collect()
    .await
}

pub(super) async fn run_validate_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    if checkpoint.findings_so_far.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("../prompts/security_engineer_validate.md");
    // One shard per up-to-STAGE_SHARD_SIZE findings; the small-review case is a
    // single child identical to before.
    let decided: BTreeSet<&str> = checkpoint
        .validation_decisions_so_far
        .iter()
        .map(|decision| decision.finding_id.as_str())
        .collect();
    let findings = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| !decided.contains(finding.id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if findings.is_empty() {
        return Ok(None);
    }
    let shards = shard_by_findings(checkpoint, &findings);
    let results = dispatch_checkpoint_shards(
        rt,
        SecurityHarnessStage::Validate,
        prompt,
        shards,
        VALIDATE_MAX_ITERATIONS,
    )
    .await;

    let mut aggregate = ToolOutput::success(String::new());
    let mut decisions = Vec::new();
    for res in results {
        // A shard transport error fails the stage, matching the single-child
        // `?` behaviour — Validate is a quality gate, not best-effort coverage.
        let (raw, stage_out) = res?;
        merge_stage_tool_output(&mut aggregate, stage_out);
        // Shape loose per shard: prose / wrong-shaped JSON drops that shard's
        // decisions, never the others.
        match parse_validate_output_shape(&raw) {
            Ok(v) => decisions.extend(v.decisions),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "validate shard output did not parse as JSON; carrying \
                     existing decisions, no new decisions from this shard"
                );
            }
        }
    }

    // Semantic strict (across ALL shards' decisions): a hallucinated finding_id,
    // a confirm missing required evidence, or a confirmed no-vulnerability note
    // fails loudly — letting it through would put junk in the report.
    let combined = ValidateStageOutput { decisions };
    validate_decisions_semantic(&combined, &checkpoint.findings_so_far)?;
    let expected: BTreeSet<&str> = findings.iter().map(|finding| finding.id.as_str()).collect();
    let mut returned = BTreeSet::new();
    for decision in &combined.decisions {
        if !expected.contains(decision.finding_id.as_str()) {
            return Err(format!(
                "validator returned finding {} outside its pending shard set",
                decision.finding_id
            ));
        }
        if !returned.insert(decision.finding_id.clone()) {
            return Err(format!(
                "validator returned duplicate decision for {}",
                decision.finding_id
            ));
        }
    }
    let mut decisions = combined.decisions;
    for finding in &findings {
        if returned.contains(&finding.id) {
            continue;
        }
        checkpoint
            .run_health
            .incomplete_results
            .push(format!("validate:{}", finding.id));
        checkpoint.coverage_gaps.push(CoverageGap {
            area: format!("Validation for {}", finding.id),
            reason: "validator returned no decision; preserved as needs_more_evidence".into(),
            risk: finding.severity.clone(),
        });
        decisions.push(super::types::ValidationDecision {
            finding_id: finding.id.clone(),
            decision: ValidationDecisionKind::NeedsMoreEvidence,
            evidence: "validator returned no usable decision".into(),
            severity: None,
        });
    }
    checkpoint.validation_decisions_so_far.extend(decisions);
    Ok(Some(aggregate))
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
    let has_confirmed = checkpoint
        .validation_decisions_so_far
        .iter()
        .any(|d| d.decision == ValidationDecisionKind::Confirmed);
    if !has_confirmed {
        return Ok(None);
    }
    let prompt = include_str!("../prompts/security_engineer_trace.md");
    // Shard the full finding set (each shard's child traces its own confirmed
    // findings); single child for a small review.
    let confirmed_ids: BTreeSet<&str> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|decision| decision.decision == ValidationDecisionKind::Confirmed)
        .map(|decision| decision.finding_id.as_str())
        .collect();
    let traced_ids: BTreeSet<&str> = checkpoint
        .trace_results_so_far
        .iter()
        .map(|trace| trace.finding_id.as_str())
        .collect();
    let findings = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| {
            confirmed_ids.contains(finding.id.as_str()) && !traced_ids.contains(finding.id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if findings.is_empty() {
        return Ok(None);
    }
    let shards = shard_by_findings(checkpoint, &findings);
    let results = dispatch_checkpoint_shards(
        rt,
        SecurityHarnessStage::Trace,
        prompt,
        shards,
        TRACE_MAX_ITERATIONS,
    )
    .await;

    let mut aggregate = ToolOutput::success(String::new());
    let mut last_parse_err: Option<String> = None;
    for res in results {
        let (raw, stage_out) = res?;
        merge_stage_tool_output(&mut aggregate, stage_out);
        match parse_stage_json::<TraceStageOutput>(&raw) {
            Ok(traces) => checkpoint.trace_results_so_far.extend(traces.traces),
            Err(err) => {
                checkpoint.coverage_gaps.push(CoverageGap {
                    area: "Trace stage".into(),
                    reason: format!(
                        "Trace shard output was not parseable JSON; continuing with existing reachability evidence: {err}"
                    ),
                    risk: "unknown".into(),
                });
                last_parse_err = Some(err);
            }
        }
    }
    complete_trace_results(checkpoint, &findings)?;
    if let Some(err) = last_parse_err {
        checkpoint.report_validation_state = ReportValidationState {
            status: "trace_unparsed".into(),
            errors: vec![err],
        };
    }
    Ok(Some(aggregate))
}

/// Enforce one trace result per newly-confirmed finding without converting a
/// missing/malformed result into `reachable = false`. Unknowns are durable and
/// visible in both coverage gaps and Run Health.
fn complete_trace_results(
    checkpoint: &mut SecurityCheckpoint,
    findings: &[SecurityFinding],
) -> std::result::Result<(), String> {
    let expected: BTreeSet<&str> = findings.iter().map(|finding| finding.id.as_str()).collect();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for trace in &checkpoint.trace_results_so_far {
        if expected.contains(trace.finding_id.as_str()) {
            *counts.entry(trace.finding_id.clone()).or_default() += 1;
        } else if !checkpoint
            .findings_so_far
            .iter()
            .any(|f| f.id == trace.finding_id)
        {
            return Err(format!(
                "trace worker referenced unknown finding_id {}",
                trace.finding_id
            ));
        }
    }
    for (id, count) in &counts {
        if *count > 1 {
            return Err(format!("trace worker returned duplicate results for {id}"));
        }
    }
    for finding in findings {
        let result = checkpoint
            .trace_results_so_far
            .iter_mut()
            .find(|trace| trace.finding_id == finding.id);
        let incomplete = match result.as_ref() {
            Some(trace) => {
                trace.reachable.is_none()
                    || (trace.reachable.is_some() && trace.evidence.is_empty())
            }
            None => true,
        };
        if !incomplete {
            continue;
        }
        if let Some(trace) = result {
            trace.reachable = None;
            if trace.evidence.is_empty() {
                trace
                    .evidence
                    .push("trace worker returned no supporting evidence".into());
            }
        } else {
            checkpoint
                .trace_results_so_far
                .push(super::types::TraceResult {
                    finding_id: finding.id.clone(),
                    reachable: None,
                    severity_effect: "unknown".into(),
                    evidence: vec!["trace worker returned no usable result".into()],
                    consumer_paths: Vec::new(),
                });
        }
        checkpoint
            .run_health
            .incomplete_results
            .push(format!("trace:{}", finding.id));
        checkpoint.coverage_gaps.push(CoverageGap {
            area: format!("Trace for {}", finding.id),
            reason: "trace worker returned no complete evidence-backed verdict".into(),
            risk: finding.severity.clone(),
        });
    }
    Ok(())
}

/// Repo-internal production-reachability judgment. Re-examines the confirmed
/// findings against in-repo prod signals — latest HEAD, deploy/config files,
/// feature flags, dead code — to decide whether each flaw is actually reachable
/// in a real deployment, and records a per-finding verdict. Non-fatal on bad
/// JSON (mirrors Trace): a missing verdict simply leaves the finding unannotated.
pub(super) async fn run_judgment_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let has_confirmed = checkpoint
        .validation_decisions_so_far
        .iter()
        .any(|d| d.decision == ValidationDecisionKind::Confirmed);
    if !has_confirmed {
        return Ok(None);
    }
    let judged_ids: BTreeSet<&str> = checkpoint
        .judgment_results
        .iter()
        .map(|judgment| judgment.finding_id.as_str())
        .collect();
    let pending = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| {
            checkpoint
                .validation_decisions_so_far
                .iter()
                .any(|decision| {
                    decision.finding_id == finding.id
                        && decision.decision == ValidationDecisionKind::Confirmed
                })
                && !judged_ids.contains(finding.id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("../prompts/security_engineer_judgment.md");
    let mut judgment_checkpoint = checkpoint.clone();
    judgment_checkpoint.findings_so_far = pending.clone();
    let (raw, stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Judgment,
        prompt,
        &judgment_checkpoint,
        JUDGMENT_MAX_ITERATIONS,
    )
    .await?;
    match parse_stage_json::<JudgmentStageOutput>(&raw) {
        Ok(out) => {
            checkpoint.judgment_results.extend(out.judgments);
        }
        Err(err) => {
            checkpoint.coverage_gaps.push(CoverageGap {
                area: "Judgment stage".into(),
                reason: format!(
                    "Judgment stage output was not parseable JSON; continuing without prod-reachability verdicts: {err}"
                ),
                risk: "unknown".into(),
            });
        }
    }
    complete_judgment_results(checkpoint, &pending)?;
    Ok(Some(stage_out))
}

/// Enforce one production judgment per pending confirmed finding. As with
/// tracing, missing evidence is represented as unknown, never unreachable.
fn complete_judgment_results(
    checkpoint: &mut SecurityCheckpoint,
    findings: &[SecurityFinding],
) -> std::result::Result<(), String> {
    let expected: BTreeSet<&str> = findings.iter().map(|finding| finding.id.as_str()).collect();
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for judgment in &checkpoint.judgment_results {
        if expected.contains(judgment.finding_id.as_str()) {
            *counts.entry(judgment.finding_id.clone()).or_default() += 1;
        } else if !checkpoint
            .findings_so_far
            .iter()
            .any(|finding| finding.id == judgment.finding_id)
        {
            return Err(format!(
                "judgment worker referenced unknown finding_id {}",
                judgment.finding_id
            ));
        }
    }
    for (id, count) in &counts {
        if *count > 1 {
            return Err(format!(
                "judgment worker returned duplicate results for {id}"
            ));
        }
    }
    for finding in findings {
        let result = checkpoint
            .judgment_results
            .iter_mut()
            .find(|judgment| judgment.finding_id == finding.id);
        let incomplete = match result.as_ref() {
            Some(judgment) => {
                judgment.reachable_in_prod.is_none() || judgment.rationale.trim().is_empty()
            }
            None => true,
        };
        if !incomplete {
            continue;
        }
        if let Some(judgment) = result {
            judgment.reachable_in_prod = None;
            if judgment.rationale.trim().is_empty() {
                judgment.rationale = "judgment worker returned no supporting rationale".into();
            }
        } else {
            checkpoint
                .judgment_results
                .push(super::types::JudgmentResult {
                    finding_id: finding.id.clone(),
                    reachable_in_prod: None,
                    rationale: "judgment worker returned no usable result".into(),
                    severity_effect: "unknown".into(),
                });
        }
        checkpoint
            .run_health
            .incomplete_results
            .push(format!("judgment:{}", finding.id));
        checkpoint.coverage_gaps.push(CoverageGap {
            area: format!("Production judgment for {}", finding.id),
            reason: "judgment worker returned no complete evidence-backed verdict".into(),
            risk: finding.severity.clone(),
        });
    }
    Ok(())
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
        if trace.reachable != Some(true) {
            continue;
        }
        let Some(finding) = checkpoint
            .findings_so_far
            .iter()
            .find(|finding| finding.id == trace.finding_id)
        else {
            continue;
        };
        for path in &trace.consumer_paths {
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
    checkpoint.report_draft = Some(reconcile_report_with_checkpoint(checkpoint, &parsed));
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
        SecurityHarnessStage::Judgment => {
            format!(
                "{} reachability verdicts",
                checkpoint.judgment_results.len()
            )
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
        SecurityHarnessStage::Judgment => 0.81,
        SecurityHarnessStage::Feedback => 0.86,
        SecurityHarnessStage::Report => 0.94,
    })
}

#[cfg(test)]
mod tests;
