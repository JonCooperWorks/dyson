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

use crate::message::{Artefact, ArtefactKind};
use crate::tool::{CheckpointEvent, ToolOutput};

use super::orchestrator::{OrchestratorConfig, OrchestratorHarness};

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
        CoverageGap, DedupeGroup, ModelMetadata, ReportValidationState, SecurityCheckpoint,
        SecurityFinding, SecurityHarnessReport, SecurityHarnessStage, SecurityTask,
        StageHistoryEntry, TargetRef, TaskStatus, TraceResult, ValidationDecision,
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
pub const SECURITY_HARNESS_VERSION: &str = "security-harness-v1";
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
                self::runtime::merge_stage_tool_output(&mut out, stage_output)
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
                out.checkpoints.push(CheckpointEvent {
                    message: format!("security_engineer: {stage} failed: {e}"),
                    progress: progress_for(*stage),
                });
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
                return Ok(err_out);
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
    let elapsed = checkpoint.updated_at.saturating_sub(started_epoch);
    out.content = render_report_markdown(&report, &checkpoint);
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
            assert!(
                out.content.contains(msg),
                "content should preserve `{msg}`"
            );
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
        assert!(out.content.contains("security_engineer: resuming checkpoint sec-bbb"));
        assert!(out.content.contains("validate failed: no JSON object found"));
        // Original error string preserved
        assert!(
            out.content
                .ends_with("validate failed: no JSON object found in stage output")
        );
    }
}
