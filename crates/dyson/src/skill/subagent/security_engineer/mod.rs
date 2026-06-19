//! Security engineer staged research harness.
//!
//! The parent-facing tool remains `security_engineer`, but the implementation
//! is no longer a single broad "review this repo" child agent. The
//! orchestrator drives a staged harness:
//!
//!   Recon → Hunt → Validate → Gapfill → Dedupe → Trace → Feedback → Report
//!
//! Each stage writes a durable JSON checkpoint under the Dyson workspace's
//! `kb/` tree. In Swarm mode that path is mirrored by the existing
//! state-file sync worker, so checkpoints survive instance
//! recreate/rollout without adding a security-specific Swarm API.
//!
//! ## Module map
//!
//! - [`types`]     — all data types (Checkpoint, Finding, Task, etc.)
//! - [`checkpoint`] — `CheckpointStore`, resume logic, time/scope/git helpers
//! - [`taxonomy`]   — vulnerability class table + class lookup/normalization
//! - [`runtime`]    — `SecurityHarnessRuntime` + `spawn_stage`
//! - [`stages`]     — eight stage runners + hunt fan-out helpers
//! - [`stack`]      — language/framework specialists + provably-moot pruning
//! - [`parse`]      — JSON extraction + stage/report schema validation
//! - [`report`]     — Markdown rendering + dedupe + reportable filtering
//!
//! ## Iteration caps
//!
//! Stage iteration caps are constants in this file (not magic numbers at
//! call sites): `RECON_MAX_ITERATIONS`, `HUNT_MAX_ITERATIONS`,
//! `VALIDATE_MAX_ITERATIONS`, `TRACE_MAX_ITERATIONS`. Recon is by far the
//! largest because it explores the whole scope before any specialist
//! hunters run.

mod checkpoint;
mod parse;
mod report;
mod runtime;
mod stack;
mod stages;
mod taxonomy;
mod types;

use crate::controller::http::SubagentEventBus;
use crate::message::{Artefact, ArtefactKind};
use crate::tool::{CheckpointEvent, ToolOutput};

use super::orchestrator::{OrchestratorConfig, OrchestratorHarness, OrchestratorInput};

use self::checkpoint::{
    CheckpointStore, git_ref_for, load_checkpoint_for_resume, make_run_id, provider_label,
    scope_for, should_stop_after, target_name_for, unix_seconds,
};
use self::report::render_report_markdown;
use self::stages::{
    progress_for, run_feedback_stage, run_gapfill_stage, run_hunt_stage, run_recon_stage,
    run_report_stage, run_trace_stage, run_validate_stage, stage_completed, stage_summary,
};

// Re-export the module's public + crate-private API at the directory root so
// external callers continue to resolve `security_engineer::Name` as before.
// `cargo check --lib` doesn't see the test/orchestrator consumers and would
// fire `unused_imports` on every line here, so silence the whole block.
#[allow(unused_imports)]
pub use self::{
    parse::validate_report_json,
    report::dedupe_findings,
    taxonomy::{VulnerabilityClassDefinition, vulnerability_taxonomy},
    types::{
        CoverageGap, DedupeGroup, ModelMetadata, ReportValidationState, RunHealth,
        SecurityCheckpoint, SecurityFinding, SecurityHarnessReport, SecurityHarnessStage,
        SecurityTask, StageHistoryEntry, TargetRef, TaskStatus, TraceResult, ValidationDecision,
        ValidationDecisionKind, VulnerabilityClassCoverage,
    },
};
#[allow(unused_imports)]
pub(crate) use self::{
    parse::{parse_report_output, parse_validate_output},
    report::{report_from_checkpoint, reportable_confirmed_findings},
    runtime::SecurityHarnessRuntime,
    stages::{resolve_repaired_or_fallback_report, run_dedupe_stage},
    types::ValidateStageOutput,
};

use crate::error::Result;

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

pub const SECURITY_HARNESS_SCHEMA_VERSION: u32 = 1;
pub const SECURITY_HARNESS_VERSION: &str = "security-harness-v2";
/// Harness versions whose checkpoints this binary can still resume. All v1→v2
/// changes are additive (`#[serde(default)]` fields, a new optional Judgment
/// stage), so a v1 checkpoint deserializes and resumes cleanly — any new stage
/// simply isn't in its `stage_history` yet and runs once. Keep the current
/// version in this list.
pub const SUPPORTED_HARNESS_VERSIONS: &[&str] = &["security-harness-v1", "security-harness-v2"];
/// Pure runaway backstop on class-specialist hunters spawned in one Hunt
/// stage.  Specialist count is driven by the work list (one per applicable
/// class), not by this number — it only stops a pathological recon that
/// emits unbounded follow-up tasks from spawning forever.  It does not bind
/// for the canonical taxonomy.
pub const HUNT_SPECIALIST_BACKSTOP: usize = 64;

/// Per-stage iteration caps on the child agent loop.  Recon was originally
/// 12, which a thorough non-Claude model (deepseek-v4-pro in practice) blew
/// through doing ~50 tool calls per turn — the loop fell into the
/// `summarize_on_max_iterations` path and returned prose instead of JSON,
/// killing the recon→hunt transition.  Hunt is already 28; recon needs at
/// least as much because it explores the WHOLE scope before the hunters
/// even start.  Validate/trace operate on a bounded checkpoint and stay
/// where they were.
pub(crate) const RECON_MAX_ITERATIONS: usize = 60;
pub(crate) const HUNT_MAX_ITERATIONS: usize = 28;
pub(crate) const VALIDATE_MAX_ITERATIONS: usize = 16;
pub(crate) const TRACE_MAX_ITERATIONS: usize = 16;

/// A stage finishing faster than this (and that runs a child) is flagged as a
/// possible shallow run in the report's Run Health section. Real LLM stages do
/// many tool calls and take far longer; a sub-2s return means the model barely
/// engaged. Purely observational — never fails the run.
const FAST_STAGE_THRESHOLD_SECS: u64 = 2;

/// Whether `stage` spawns a child agent (the deterministic bookkeeping stages —
/// Gapfill/Dedupe/Feedback — legitimately finish instantly and must not be
/// flagged as shallow).
fn is_llm_backed_stage(stage: SecurityHarnessStage) -> bool {
    matches!(
        stage,
        SecurityHarnessStage::Recon
            | SecurityHarnessStage::Hunt
            | SecurityHarnessStage::Validate
            | SecurityHarnessStage::Trace
            | SecurityHarnessStage::Report
    )
}

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
        description: "Runs a staged, vulnerability-class-driven security research harness with \
             durable checkpoints: recon, taxonomy-based hunt batches, independent validation, \
             gapfill, dedupe, reachability tracing, feedback tasks, and schema-checked reporting. \
             Use for scoped authorized reviews and for resuming prior security_engineer \
             checkpoints.",
        system_prompt: include_str!("../prompts/security_engineer.md"),
        direct_tool_names: DIRECT_TOOLS,
        // The staged harness uses smaller per-stage child budgets internally;
        // this remains the advertised ceiling for legacy metadata/tests and
        // as an upper bound for any single security stage child.
        max_iterations: 80,
        max_tokens: 8192,
        injects_protocol: Some(include_str!("../prompts/security_engineer_protocol.md")),
        emit_artefact: Some(ArtefactKind::SecurityReview),
        harness: Some(OrchestratorHarness::SecurityResearch),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn harness_stages() -> &'static [SecurityHarnessStage] {
    STAGES
}

/// Per-stage model overrides, sourced from `DYSON_SEC_<STAGE>_MODEL` env vars.
///
/// Pinning a different model to a later stage than Hunt is the cheapest path to
/// Cloudflare-VVS-style cross-model separation: the run's findings get judged by
/// different weights than the ones that produced them. The override reuses the
/// run's provider + client (a model is just a per-request string), so this is
/// same-provider only — cross-provider would need a second authenticated
/// client. Empty or unset vars fall back to the run model. Mirrors the env-key
/// fallback pattern in `llm::registry::resolve_api_key`.
pub(crate) fn resolve_stage_models() -> std::collections::BTreeMap<SecurityHarnessStage, String> {
    resolve_stage_models_from(|var| std::env::var(var).ok())
}

/// Only the LLM-backed stages are listed; Gapfill/Dedupe/Feedback run no child,
/// so an override would be inert.
const STAGE_MODEL_ENV: &[(SecurityHarnessStage, &str)] = &[
    (SecurityHarnessStage::Recon, "DYSON_SEC_RECON_MODEL"),
    (SecurityHarnessStage::Hunt, "DYSON_SEC_HUNT_MODEL"),
    (SecurityHarnessStage::Validate, "DYSON_SEC_VALIDATE_MODEL"),
    (SecurityHarnessStage::Trace, "DYSON_SEC_TRACE_MODEL"),
    (SecurityHarnessStage::Report, "DYSON_SEC_REPORT_MODEL"),
];

/// Pure core of [`resolve_stage_models`], taking the env lookup as a closure so
/// it can be unit-tested without mutating process-global environment (which is
/// racy under the parallel test runner and `unsafe` on edition 2024).
fn resolve_stage_models_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> std::collections::BTreeMap<SecurityHarnessStage, String> {
    let mut out = std::collections::BTreeMap::new();
    for (stage, var) in STAGE_MODEL_ENV {
        if let Some(model) = lookup(var) {
            let model = model.trim();
            if !model.is_empty() {
                out.insert(*stage, model.to_string());
            }
        }
    }
    out
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

    let mut result = run_security_harness_inner(&rt, started_epoch).await;

    // Persist the CheckpointEvent stream into out.content so the
    // SecurityHarnessPanel can rebuild stage state on rehydrate.
    //
    // During a live run, CheckpointEvents flow as SSE events; the
    // frontend's onCheckpoint callback appends each message to the
    // live tool's body.text in React state.  When the conversation is
    // persisted, only the ToolResult `content` field survives — the
    // CheckpointEvent stream is dropped.  Page refresh rehydrates
    // body.text from `content`, which (pre-fix) was empty of stage
    // events, so the panel rendered "(no run id yet)" even for a
    // completed run.
    //
    // Fix: prepend an HTML comment block carrying every event message
    // to the content.  HTML comments are stripped by Markdown
    // renderers, so the visible report stays clean.  The panel's
    // parser splits body.text on '\n' and matches `security_engineer:`
    // lines regardless of position, so finds them inside the comment.
    //
    // Applies to both success and error returns — failure runs still
    // benefit from the panel surfacing the failure-stage badge after
    // refresh.
    if let Ok(out) = &mut result {
        bake_checkpoint_events_into_content(out);
    }

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

/// Prepend an HTML-comment-wrapped dump of `out.checkpoints` to
/// `out.content`.  No-op when there are zero CheckpointEvents (early
/// pre-`load_for_resume` errors).  The comment markers anchor the
/// block so the panel parser can find it on rehydrate even after
/// future content rewrites.
fn bake_checkpoint_events_into_content(out: &mut ToolOutput) {
    if out.checkpoints.is_empty() {
        return;
    }
    let mut header = String::from("<!-- security-harness-events\n");
    for cp in &out.checkpoints {
        header.push_str(&cp.message);
        header.push('\n');
    }
    header.push_str("-->\n");
    // Combine without dropping content — the prefix lives at the top
    // of the rendered Markdown but is invisible after HTML-comment
    // stripping.
    out.content = format!("{header}{}", out.content);
}

/// Prepend an authoritative panel-state JSON snapshot to `out.content`.
///
/// The events bake above is a list of `security_engineer: ...` log lines
/// the SecurityHarnessPanel has to re-derive state from.  Live runs got
/// the event stream straight from the SubagentEventBus and rendered fine,
/// but rehydrate-after-refresh repeatedly produced a degraded panel:
/// stages survived, run-id / findings counter / class grid did not.
/// Diagnosed cause: which subset of events ends up in `body.text` after
/// rehydrate is sensitive to the order of `hydrateTranscript`, SSE
/// replay, `applyToolView`, and `appendToolText` — multiple paths write
/// or partially overwrite the same field, and only stage-name lines
/// consistently survived because they appear in every emission path.
///
/// A single JSON snapshot eliminates that fragility.  Every panel field
/// the user cares about — run id, completed flag, findings rollup, class
/// status, failure stage — lands in one comment block that downstream
/// code can't accidentally partially drop.  The frontend prefers this
/// snapshot when present and falls back to event parsing for live runs
/// (where the snapshot hasn't been emitted yet) and for historical
/// content from before snapshots existed.
fn bake_panel_state_snapshot(
    out: &mut ToolOutput,
    checkpoint: &SecurityCheckpoint,
    failure: Option<(SecurityHarnessStage, &str)>,
) {
    let snapshot = panel_state_snapshot(checkpoint, failure);
    // Defang `-->` inside the JSON payload: any unescaped occurrence
    // would close the host HTML comment early and truncate the
    // snapshot.  Replacing every `>` with its `>` escape is a
    // no-op semantically (serde decodes it back to `>`) but it
    // structurally forbids the closing-comment sequence.  Cheaper
    // than scanning for the specific `-->` triple and matches the
    // approach the frontend extractor uses.
    let safe = snapshot.replace('>', "\\u003e");
    out.content = format!("<!-- security-harness-state {safe} -->\n{}", out.content);
}

/// Build the JSON payload embedded by [`bake_panel_state_snapshot`].
///
/// Mirrors what the SecurityHarnessPanel needs to render, NOT the
/// full SecurityCheckpoint — the snapshot is a UI contract, not a
/// checkpoint dump.  Adding fields here is cheap; the panel ignores
/// keys it doesn't know about, so this struct can grow without
/// breaking older frontends.
fn panel_state_snapshot(
    checkpoint: &SecurityCheckpoint,
    failure: Option<(SecurityHarnessStage, &str)>,
) -> String {
    let r = self::types::SeverityRollup::from_findings(&checkpoint.findings_so_far);
    let mut class_status = serde_json::Map::new();
    for cov in &checkpoint.class_coverage {
        if cov.class_id.is_empty() {
            continue;
        }
        // Mirror the live `hunt: class <id> <status>` event vocabulary
        // the panel already understands.  `hunted` carries a per-class
        // finding count (counted from findings_so_far rather than from
        // the cov struct because the panel renders the same way).
        let (status, count) = if cov.hunted {
            let n = checkpoint
                .findings_so_far
                .iter()
                .filter(|f| f.vulnerability_class == cov.class_id)
                .count();
            ("hunted", n as u64)
        } else if cov.checked_and_cleared {
            ("cleared", 0)
        } else if cov.considered && !cov.applicable {
            ("inapplicable", 0)
        } else {
            continue;
        };
        class_status.insert(
            cov.class_id.clone(),
            serde_json::json!({ "status": status, "count": count }),
        );
    }
    let payload = serde_json::json!({
        "run_id": checkpoint.run_id,
        "completed": checkpoint.completed,
        "failed_at_stage": failure.map(|(s, _)| s.as_str()),
        "failure_message": failure.map(|(_, m)| m),
        "findings": {
            "critical": r.critical,
            "high": r.high,
            "medium": r.medium,
            "low": r.low,
        },
        "class_status": class_status,
    });
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
}

/// Whether a run should resume an existing durable checkpoint.
///
/// Resume intent comes *only* from the explicit `resume` flag or a non-empty
/// `run_id` — never from the free-form `task` text.  A fresh run can
/// legitimately describe itself as "NOT a resume from any checkpoint", and a
/// prior bare-substring match on the task ("resume") forced those fresh runs
/// into the resume path, which then hard-errored with "no incomplete
/// checkpoint found".  Every real resume caller already sets the flag/run_id.
fn should_resume(parsed: &OrchestratorInput) -> bool {
    parsed.resume || parsed.run_id.as_ref().is_some_and(|s| !s.trim().is_empty())
}

/// Push a harness checkpoint onto the aggregate output AND stream it to the
/// live subagent panel via the event bus.
///
/// `out.checkpoints` alone only reaches the frontend when the tool returns
/// (and on rehydrate via the baked content block), so without the live emit
/// the SecurityHarnessPanel's StageBar sits on "initializing — (no run id
/// yet)" for the entire run instead of advancing through run_id, stage, and
/// findings in real time.  The frontend appends `checkpoint` events to the
/// live tool's body text, which stays pinned to this panel for the run.
fn emit_checkpoint(
    out: &mut ToolOutput,
    events: Option<&SubagentEventBus>,
    event: CheckpointEvent,
) {
    if let Some(bus) = events {
        bus.checkpoint(&event.message);
    }
    out.checkpoints.push(event);
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
    };

    let resumed = should_resume(&rt.parsed);
    let mut checkpoint = if resumed {
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
    emit_checkpoint(
        &mut out,
        rt.events.as_ref(),
        CheckpointEvent {
            message: format!(
                "security_engineer: {} checkpoint {}",
                if resumed { "resuming" } else { "created" },
                checkpoint.run_id
            ),
            progress: Some(0.02),
        },
    );

    for stage in STAGES {
        if stage_completed(&checkpoint, *stage) {
            continue;
        }
        checkpoint.current_stage = *stage;
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        if let Err(e) = store.save(&checkpoint).await {
            return Ok(ToolOutput::error(e));
        }
        emit_checkpoint(
            &mut out,
            rt.events.as_ref(),
            CheckpointEvent {
                message: format!("security_engineer: {stage}"),
                progress: progress_for(*stage),
            },
        );

        let stage_started = unix_seconds(std::time::SystemTime::now());
        let stage_result = match stage {
            SecurityHarnessStage::Recon => run_recon_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Hunt => run_hunt_stage(rt, &store, &mut checkpoint).await,
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
            Ok(Some(stage_output)) => {
                // Stream the stage's own checkpoints (findings counts,
                // per-class hunt outcomes) live as the stage lands, so the
                // FindingsCounter / ClassGrid advance during the run rather
                // than only at completion.
                let before = out.checkpoints.len();
                self::runtime::merge_stage_tool_output(&mut out, stage_output);
                if let Some(bus) = rt.events.as_ref() {
                    for cp in &out.checkpoints[before..] {
                        bus.checkpoint(&cp.message);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                checkpoint.report_validation_state = ReportValidationState {
                    status: "failed".into(),
                    errors: vec![e.clone()],
                };
                checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
                let _ = store.save(&checkpoint).await;
                // Emit a final CheckpointEvent carrying the per-stage failure
                // message so the frontend's SecurityHarnessPanel can render an
                // "errored at <stage>" badge + error banner.  Before this push,
                // the failure was only visible on the outer tool chip's
                // is_error flag — the panel itself had no signal to read.
                emit_checkpoint(
                    &mut out,
                    rt.events.as_ref(),
                    CheckpointEvent {
                        message: format!("security_engineer: {stage} failed: {e}"),
                        progress: progress_for(*stage),
                    },
                );
                let mut err_out = ToolOutput::error(format!(
                    "security_engineer {stage} failed: {e}. checkpoint={}",
                    checkpoint.run_id
                ));
                // Carry the accumulated stream + child metadata onto the
                // error output so the frontend sees the same history it
                // would have seen for a successful return.
                err_out.checkpoints = out.checkpoints;
                err_out.artefacts = out.artefacts;
                err_out.metadata = out.metadata;
                bake_panel_state_snapshot(&mut err_out, &checkpoint, Some((*stage, e.as_str())));
                return Ok(err_out);
            }
        }

        let stage_finished = unix_seconds(std::time::SystemTime::now());
        // Health signal: an LLM-backed stage that returns almost instantly
        // probably did little real work (a shallow recon/hunt, a validator that
        // rubber-stamped). The deterministic stages are legitimately instant, so
        // they are excluded. Recorded for the report, never fatal.
        let elapsed = stage_finished.saturating_sub(stage_started);
        if is_llm_backed_stage(*stage) && elapsed < FAST_STAGE_THRESHOLD_SECS {
            checkpoint
                .run_health
                .fast_stages
                .push(format!("{stage}:{elapsed}s"));
        }
        checkpoint.stage_history.push(StageHistoryEntry {
            stage: *stage,
            status: "completed".into(),
            started_at: stage_started,
            finished_at: stage_finished,
            summary: stage_summary(&checkpoint, *stage),
            model: rt.stage_model(*stage).to_string(),
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

    emit_checkpoint(
        &mut out,
        rt.events.as_ref(),
        CheckpointEvent {
            message: format!(
                "security_engineer: health degraded={} requeued={} fast_stages={}",
                checkpoint.run_health.degraded_specialists,
                checkpoint.run_health.requeued_classes.len(),
                checkpoint.run_health.fast_stages.len(),
            ),
            progress: Some(0.97),
        },
    );

    let report = checkpoint
        .report_draft
        .clone()
        .unwrap_or_else(|| report_from_checkpoint(&checkpoint));
    let elapsed = checkpoint.updated_at.saturating_sub(started_epoch);
    out.content = render_report_markdown(&report, &checkpoint);
    emit_checkpoint(
        &mut out,
        rt.events.as_ref(),
        CheckpointEvent {
            message: format!(
                "security_engineer: completed {} in {}s",
                checkpoint.run_id, elapsed
            ),
            progress: Some(1.0),
        },
    );

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
            // Per-stage overrides (empty when every stage ran the run model).
            // Lets the artefact reader see e.g. validate judged on a different
            // model than hunt without parsing the stage-history table.
            "stage_models": serde_json::to_value(&rt.stage_models).unwrap_or_default(),
            "checkpoint_path": checkpoint.checkpoint_path(),
            "stage_count": checkpoint.stage_history.len(),
        });
        out.artefacts
            .push(Artefact::markdown(kind, title, out.content.clone()).with_metadata(metadata));
    }

    // Bake AFTER the artefact captures `out.content` so the persisted
    // markdown artefact stays clean — only the live tool body carries
    // the snapshot comment.
    bake_panel_state_snapshot(&mut out, &checkpoint, None);

    Ok(out)
}

#[cfg(test)]
mod specialist_tests {
    use self::stack::{
        class_provably_inapplicable, prune_inapplicable_class_tasks, stack_specialists,
    };
    use self::stages::{distinct_pending_classes, pending_tasks_for_class};
    use self::taxonomy::{VULNERABILITY_TAXONOMY, class_ast_hints, class_specialist_briefing};
    use super::*;
    use crate::skill::subagent::repo_detect::{self, Detection};

    #[test]
    fn every_taxonomy_class_has_a_specialist_briefing() {
        for class in VULNERABILITY_TAXONOMY {
            let briefing = class_specialist_briefing(class.id)
                .unwrap_or_else(|| panic!("no briefing for class {}", class.id));
            assert!(
                briefing.contains(class.name),
                "briefing for {} missing class name",
                class.id
            );
            // Evidence requirements must be carried into the briefing so the
            // specialist knows what to collect before reporting.
            assert!(
                briefing.contains(class.evidence_requirements[0]),
                "briefing for {} missing evidence requirements",
                class.id
            );
        }
    }

    #[test]
    fn follow_up_classes_fall_back_to_generic_prompt() {
        // Tasks generated mid-run (e.g. consumer_path_review) are not in the
        // taxonomy and must not produce a specialist briefing.
        assert!(class_specialist_briefing("consumer_path_review").is_none());
        assert!(class_specialist_briefing("").is_none());
    }

    #[test]
    fn resume_decision_ignores_task_text() {
        use serde_json::json;
        let parse = |v| serde_json::from_value::<OrchestratorInput>(v).unwrap();

        // Regression for the c-0056 fresh-run failure: a fresh review whose
        // task merely mentions the word "resume" (even a negation) must NOT be
        // routed into the resume path, which hard-errors when no checkpoint
        // exists with "no incomplete checkpoint found".
        assert!(!should_resume(&parse(json!({
            "task": "Run a FRESH security review, NOT a resume from any checkpoint"
        }))));
        assert!(!should_resume(&parse(
            json!({"task": "resume security review"})
        )));

        // Resume intent comes only from the explicit flag or a real run_id.
        assert!(should_resume(&parse(json!({"resume": true}))));
        assert!(should_resume(&parse(json!({"run_id": "sec-123-4"}))));

        // An empty / whitespace run_id is not a resume request.
        assert!(!should_resume(&parse(json!({"run_id": ""}))));
        assert!(!should_resume(&parse(json!({"run_id": "   "}))));
        assert!(!should_resume(&parse(json!({}))));
    }

    #[test]
    fn resolve_stage_models_trims_and_skips_blank_or_unset() {
        let envs = std::collections::HashMap::from([
            ("DYSON_SEC_VALIDATE_MODEL", "  openrouter/judge-model  "),
            ("DYSON_SEC_REPORT_MODEL", "   "), // blank → must be skipped
        ]);
        let map = super::resolve_stage_models_from(|var| envs.get(var).map(|s| s.to_string()));
        assert_eq!(
            map.get(&SecurityHarnessStage::Validate).map(String::as_str),
            Some("openrouter/judge-model"),
            "set var should be trimmed and recorded"
        );
        assert!(
            !map.contains_key(&SecurityHarnessStage::Report),
            "a blank env value must not create an override (would pin an empty model)"
        );
        assert!(
            !map.contains_key(&SecurityHarnessStage::Hunt),
            "an unset stage stays on the run default"
        );
    }

    #[test]
    fn resolve_stage_models_only_covers_llm_stages() {
        // Even when every var is set, only the LLM-backed stages get overrides;
        // the deterministic stages (Gapfill/Dedupe/Feedback) spawn no child.
        let all = super::resolve_stage_models_from(|_| Some("x".to_string()));
        for stage in [
            SecurityHarnessStage::Recon,
            SecurityHarnessStage::Hunt,
            SecurityHarnessStage::Validate,
            SecurityHarnessStage::Trace,
            SecurityHarnessStage::Report,
        ] {
            assert!(all.contains_key(&stage), "{stage} should be overridable");
        }
        for stage in [
            SecurityHarnessStage::Gapfill,
            SecurityHarnessStage::Dedupe,
            SecurityHarnessStage::Feedback,
        ] {
            assert!(
                !all.contains_key(&stage),
                "{stage} is deterministic and must not be overridable"
            );
        }
    }

    #[test]
    fn high_leverage_classes_carry_ast_patterns() {
        // Classes where AST querying is the primary technique must ship
        // concrete patterns so hunters do not silently fall back to grep.
        for id in [
            "auth_authorization",
            "injection_unsafe_execution",
            "ssrf_outbound_network",
            "file_archive_path",
        ] {
            assert!(
                !class_ast_hints(id).is_empty(),
                "expected AST hints for {id}"
            );
        }
    }

    #[test]
    fn stack_specialists_cover_detected_stack() {
        let detection = Detection {
            languages: vec![repo_detect::Language::Rust],
            frameworks: vec![repo_detect::Framework::Axum],
        };
        let specs = stack_specialists(&detection);
        let labels: Vec<&str> = specs.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"lang/rust"), "labels: {labels:?}");
        assert!(labels.contains(&"framework/axum"), "labels: {labels:?}");
        // Each specialist's briefing embeds its own reference content, so the
        // knowledge rides in that agent's own context — not a shared prompt.
        let axum = specs.iter().find(|s| s.label == "framework/axum").unwrap();
        assert!(axum.briefing.contains("framework/axum"));
        assert!(
            axum.briefing.len() > 200,
            "briefing should embed the reference content"
        );
    }

    #[test]
    fn stack_specialists_empty_for_unknown_stack() {
        assert!(stack_specialists(&Detection::default()).is_empty());
    }

    #[test]
    fn supply_chain_pruned_only_when_no_manifests() {
        assert!(class_provably_inapplicable(
            "dependency_supply_chain",
            &Detection::default()
        ));
        let with_lang = Detection {
            languages: vec![repo_detect::Language::Rust],
            frameworks: vec![],
        };
        assert!(!class_provably_inapplicable(
            "dependency_supply_chain",
            &with_lang
        ));
        // Behavior-dependent classes are never pruned, even on an empty stack.
        for id in [
            "auth_authorization",
            "injection_unsafe_execution",
            "crypto_randomness",
        ] {
            assert!(
                !class_provably_inapplicable(id, &Detection::default()),
                "{id} must never be pruned"
            );
        }
    }

    #[test]
    fn distinct_pending_classes_dedupes_in_order() {
        let mut cp = SecurityCheckpoint::new(
            "run".into(),
            TargetRef {
                repo_path: ".".into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        );
        for (id, class) in [
            ("t1", "auth_authorization"),
            ("t2", "auth_authorization"),
            ("t3", "injection_unsafe_execution"),
            ("t4", "auth_authorization"),
        ] {
            cp.pending_tasks.push(SecurityTask {
                id: id.into(),
                attack_class: class.into(),
                scope_hint: "s".into(),
                status: TaskStatus::Pending,
                rationale: String::new(),
            });
        }
        // One wave dispatches one specialist per distinct class, first-seen
        // order — so auth (t1,t2,t4) is a single specialist, not three.
        assert_eq!(
            distinct_pending_classes(&cp),
            vec![
                "auth_authorization".to_string(),
                "injection_unsafe_execution".to_string()
            ]
        );
        assert_eq!(pending_tasks_for_class(&cp, "auth_authorization").len(), 3);
    }

    #[test]
    fn prune_moves_inapplicable_task_to_completed() {
        let mut cp = SecurityCheckpoint::new(
            "run".into(),
            TargetRef {
                repo_path: ".".into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        );
        cp.pending_tasks.push(SecurityTask {
            id: "t1".into(),
            attack_class: "dependency_supply_chain".into(),
            scope_hint: "deps".into(),
            status: TaskStatus::Pending,
            rationale: String::new(),
        });
        cp.pending_tasks.push(SecurityTask {
            id: "t2".into(),
            attack_class: "auth_authorization".into(),
            scope_hint: "auth".into(),
            status: TaskStatus::Pending,
            rationale: String::new(),
        });
        prune_inapplicable_class_tasks(&mut cp, &Detection::default());
        assert_eq!(cp.pending_tasks.len(), 1);
        assert_eq!(cp.pending_tasks[0].attack_class, "auth_authorization");
        assert_eq!(cp.completed_tasks.len(), 1);
        assert_eq!(
            cp.completed_tasks[0].attack_class,
            "dependency_supply_chain"
        );
    }

    // ---- Phase 4: rehydrate fix ------------------------------------------

    #[test]
    fn bake_checkpoint_events_prepends_html_comment_block_to_content() {
        use crate::tool::CheckpointEvent;
        let mut out = ToolOutput::success("# Security review\n\n## CRITICAL\n");
        out.checkpoints.push(CheckpointEvent {
            message: "security_engineer: resuming checkpoint sec-aaa".into(),
            progress: Some(0.02),
        });
        out.checkpoints.push(CheckpointEvent {
            message: "security_engineer: validate".into(),
            progress: Some(0.55),
        });
        out.checkpoints.push(CheckpointEvent {
            message: "security_engineer: completed sec-aaa in 99s".into(),
            progress: Some(1.0),
        });

        super::bake_checkpoint_events_into_content(&mut out);

        // HTML comment block is at the top
        assert!(
            out.content.starts_with("<!-- security-harness-events\n"),
            "comment block should be at the top: {:?}",
            &out.content[..80]
        );
        // Every event message is preserved
        for msg in &[
            "security_engineer: resuming checkpoint sec-aaa",
            "security_engineer: validate",
            "security_engineer: completed sec-aaa in 99s",
        ] {
            assert!(out.content.contains(msg), "content should preserve `{msg}`");
        }
        // Original content survives
        assert!(out.content.contains("# Security review"));
        assert!(out.content.contains("## CRITICAL"));
        // Comment is well-formed
        assert!(out.content.contains("-->\n"));
    }

    #[test]
    fn bake_checkpoint_events_is_noop_when_stream_is_empty() {
        let mut out = ToolOutput::success("plain content");
        assert!(out.checkpoints.is_empty());
        super::bake_checkpoint_events_into_content(&mut out);
        assert_eq!(out.content, "plain content");
    }

    #[test]
    fn bake_checkpoint_events_also_applies_to_error_outputs() {
        // The failure path in run_security_harness_inner carries the
        // accumulated checkpoint stream onto the error output before
        // returning.  We bake on both Ok-success and Ok-error so the
        // panel can rehydrate `failed at <stage>` state on a refresh
        // of a failed run.
        use crate::tool::CheckpointEvent;
        let mut out = ToolOutput::error("validate failed: no JSON object found in stage output");
        out.checkpoints.push(CheckpointEvent {
            message: "security_engineer: resuming checkpoint sec-bbb".into(),
            progress: Some(0.02),
        });
        out.checkpoints.push(CheckpointEvent {
            message: "security_engineer: validate failed: no JSON object found in stage output"
                .into(),
            progress: Some(0.55),
        });

        super::bake_checkpoint_events_into_content(&mut out);
        assert!(out.is_error);
        assert!(
            out.content
                .contains("security_engineer: resuming checkpoint sec-bbb")
        );
        assert!(
            out.content
                .contains("validate failed: no JSON object found")
        );
        // Original error string preserved
        assert!(
            out.content
                .ends_with("validate failed: no JSON object found in stage output")
        );
    }

    // ---- Phase 5: structured panel-state snapshot -------------------------
    //
    // The events bake above gives the panel a free-form log to reparse on
    // rehydrate, but the panel-state QA on 2026-06-08 showed that subset
    // of events survives the hydrate → SSE-replay → applyToolView chain
    // is fragile: stage glyphs reliably come back, run-id / findings
    // counter / class grid often don't.  The snapshot below is the
    // authoritative state — one JSON blob the frontend can consume
    // without re-deriving anything from the event stream.

    fn fixture_checkpoint() -> SecurityCheckpoint {
        SecurityCheckpoint::new(
            "sec-1780939724-2".into(),
            TargetRef {
                repo_path: "/tmp/vuln".into(),
                git_ref: None,
            },
            "/tmp/vuln".into(),
            ModelMetadata {
                provider: "openrouter".into(),
                model: "deepseek/deepseek-v4-flash".into(),
            },
            0,
        )
    }

    fn finding(severity: &str, class: &str) -> SecurityFinding {
        SecurityFinding {
            id: format!("finding-{class}-{severity}"),
            title: format!("{severity} {class}"),
            severity: severity.into(),
            vulnerability_class: class.into(),
            ..Default::default()
        }
    }

    fn parse_snapshot(content: &str) -> serde_json::Value {
        // Extract the JSON payload from `<!-- security-harness-state {...} -->`.
        // Equivalent to the frontend's regex: every `>` inside the
        // payload is escaped to `>` by the bake, so the next ` -->`
        // is unambiguously the closer.
        let start = content
            .find("<!-- security-harness-state ")
            .expect("snapshot comment missing");
        let after = &content[start + "<!-- security-harness-state ".len()..];
        let end = after.find(" -->").expect("snapshot closer missing");
        serde_json::from_str(&after[..end]).expect("snapshot payload should be valid JSON")
    }

    #[test]
    fn bake_panel_snapshot_emits_run_id_completed_and_severity_rollup() {
        let mut cp = fixture_checkpoint();
        cp.completed = true;
        cp.findings_so_far = vec![
            finding("critical", "injection_unsafe_execution"),
            finding("critical", "secrets_credentials"),
            finding("high", "auth_authorization"),
            finding("medium", "crypto_randomness"),
            finding("low", "audit_observability_forensics"),
            // "info" / "informational" both fold into low — the panel
            // doesn't surface a separate info bucket.
            finding("info", "frontend_security_ux"),
            finding("informational", "data_retention_privacy"),
        ];

        let mut out = ToolOutput::success("# Security review\n");
        super::bake_panel_state_snapshot(&mut out, &cp, None);

        // Sanity: snapshot prepends as a single HTML comment line at the
        // very top, so prior `out.content` survives unchanged below it.
        assert!(out.content.starts_with("<!-- security-harness-state "));
        assert!(out.content.contains("# Security review"));

        let snap = parse_snapshot(&out.content);
        assert_eq!(snap["run_id"], "sec-1780939724-2");
        assert_eq!(snap["completed"], true);
        assert!(snap["failed_at_stage"].is_null());
        assert!(snap["failure_message"].is_null());
        assert_eq!(snap["findings"]["critical"], 2);
        assert_eq!(snap["findings"]["high"], 1);
        assert_eq!(snap["findings"]["medium"], 1);
        // info + informational + low merge into the low bucket — three findings.
        assert_eq!(snap["findings"]["low"], 3);
    }

    #[test]
    fn bake_panel_snapshot_emits_class_status_keyed_by_class_id() {
        let mut cp = fixture_checkpoint();
        cp.findings_so_far = vec![
            finding("critical", "injection_unsafe_execution"),
            finding("high", "injection_unsafe_execution"),
            finding("critical", "secrets_credentials"),
        ];
        cp.class_coverage = vec![
            VulnerabilityClassCoverage {
                class_id: "injection_unsafe_execution".into(),
                hunted: true,
                ..Default::default()
            },
            VulnerabilityClassCoverage {
                class_id: "secrets_credentials".into(),
                hunted: true,
                ..Default::default()
            },
            VulnerabilityClassCoverage {
                class_id: "ssrf_outbound_network".into(),
                checked_and_cleared: true,
                ..Default::default()
            },
            VulnerabilityClassCoverage {
                class_id: "container_sandbox_runtime".into(),
                considered: true,
                applicable: false,
                ..Default::default()
            },
            // Classes that weren't considered shouldn't pollute the panel grid.
            VulnerabilityClassCoverage {
                class_id: "race_condition_toctou".into(),
                ..Default::default()
            },
            // Empty class_id rows must be skipped — they'd render as a
            // blank chip in the grid.
            VulnerabilityClassCoverage {
                class_id: String::new(),
                hunted: true,
                ..Default::default()
            },
        ];

        let mut out = ToolOutput::success(String::new());
        super::bake_panel_state_snapshot(&mut out, &cp, None);
        let cls = &parse_snapshot(&out.content)["class_status"];

        // Hunted classes carry the count from findings_so_far attribution.
        assert_eq!(cls["injection_unsafe_execution"]["status"], "hunted");
        assert_eq!(cls["injection_unsafe_execution"]["count"], 2);
        assert_eq!(cls["secrets_credentials"]["status"], "hunted");
        assert_eq!(cls["secrets_credentials"]["count"], 1);

        // Cleared / inapplicable surface without a count.
        assert_eq!(cls["ssrf_outbound_network"]["status"], "cleared");
        assert_eq!(cls["ssrf_outbound_network"]["count"], 0);
        assert_eq!(cls["container_sandbox_runtime"]["status"], "inapplicable");

        // Unconsidered + empty-id entries must NOT appear.
        assert!(cls.get("race_condition_toctou").is_none());
        // Empty-string class_id row is suppressed too.
        assert!(cls.as_object().unwrap().keys().all(|k| !k.is_empty()));
    }

    #[test]
    fn bake_panel_snapshot_records_failure_stage_for_stage_errors() {
        let mut cp = fixture_checkpoint();
        // Run failed before completing — completed flag stays false, and
        // the panel needs to render an "errored at validate" badge.
        cp.completed = false;

        let mut err_out = ToolOutput::error(
            "security_engineer validate failed: no JSON object found in stage output",
        );
        super::bake_panel_state_snapshot(
            &mut err_out,
            &cp,
            Some((
                SecurityHarnessStage::Validate,
                "no JSON object found in stage output",
            )),
        );

        let snap = parse_snapshot(&err_out.content);
        assert_eq!(snap["completed"], false);
        assert_eq!(snap["failed_at_stage"], "validate");
        assert_eq!(
            snap["failure_message"],
            "no JSON object found in stage output"
        );
        // The original error text must remain readable in the body too —
        // the comment-stripping markdown renderer leaves it intact.
        assert!(err_out.content.contains("validate failed"));
    }

    #[test]
    fn bake_panel_snapshot_payload_is_valid_json_even_with_quote_heavy_failure_message() {
        // Failure messages can come straight from a tool's stderr — they
        // routinely contain quotes, braces, backslashes.  serde_json
        // handles escaping; this test pins that nothing in the
        // bake/serialize path manually splices the message into JSON.
        let cp = fixture_checkpoint();
        let mut out = ToolOutput::error("oops");
        let nasty = r#"parse error: unexpected token `"--><script>alert(1)</script>` at line 4"#;
        super::bake_panel_state_snapshot(
            &mut out,
            &cp,
            Some((SecurityHarnessStage::Validate, nasty)),
        );

        let snap = parse_snapshot(&out.content);
        assert_eq!(snap["failure_message"], nasty);
        // Round-tripping the snapshot through serde must produce
        // identical bytes — an unescaped `-->` would have closed the
        // host HTML comment and broken extraction.
        let raw = serde_json::to_string(&snap).unwrap();
        let again: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(snap, again);
    }
}
