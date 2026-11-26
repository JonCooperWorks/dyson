//! The Hunt stage: per-class hunt dispatch and folding hunt results back
//! into the checkpoint.

use super::*;

pub(in super::super) async fn run_hunt_stage(
    rt: &SecurityHarnessRuntime,
    store: &CheckpointStore,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("../../prompts/security_engineer_hunt.md");
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
    // Stack specialists are a broad augmentation pass and should run only on
    // the first Hunt stage. Follow-up Hunt calls execute the newly queued
    // class-scoped work without duplicating the full language/framework pass.
    let stack = if stage_completed(checkpoint, SecurityHarnessStage::Hunt) {
        Vec::new()
    } else {
        stack_specialists(&detection)
    };
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
pub(in super::super) struct HuntDispatch {
    pub label: String,
    pub stage_prompt: String,
    pub checkpoint: SecurityCheckpoint,
    pub batch_ids: Vec<String>,
    pub is_class: bool,
}

/// One dispatched specialist paired with its child result (`(raw_json,
/// tool_output)` on success, error string on failure).
pub(in super::super) type HuntOutcome = (
    HuntDispatch,
    std::result::Result<(String, ToolOutput), String>,
);

/// Distinct attack classes among the still-pending tasks, in first-seen order.
pub(in super::super) fn distinct_pending_classes(checkpoint: &SecurityCheckpoint) -> Vec<String> {
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
pub(in super::super) async fn dispatch_hunts(
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
    .buffer_unordered(stage_concurrency())
    .collect()
    .await
}

/// The error string a Hunt specialist gets when it blows [`HUNT_CHILD_TIMEOUT`].
/// Folded as a coverage gap, never fatal — factored out so the message format
/// is unit-testable without spawning a real child.
pub(in super::super) fn hunt_child_timeout_error(label: &str) -> String {
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
pub(in super::super) fn fold_hunt_wave(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    results: Vec<HuntOutcome>,
) {
    // Read the opt-in once per wave, not once per degraded specialist.
    let requeue_enabled = requeue_shallow_enabled();
    for (d, res) in results {
        match res {
            Ok((raw, stage_out)) => {
                fold_hunt_result(checkpoint, aggregate, &d, raw, stage_out);
            }
            Err(e) => {
                fold_hunt_degraded(checkpoint, aggregate, &d, &e, requeue_enabled);
            }
        }
    }
}

/// Fold a specialist that did not complete — a transport error, a timeout, a
/// spawn failure, or malformed output. Shared by [`fold_hunt_result`] so every
/// unusable response gets identical retry and coverage-gap bookkeeping:
/// mark the specialist's batch complete (so a still-pending class is not
/// re-dispatched into an infinite loop), record a coverage gap so the report
/// surfaces the blind spot honestly, and emit a panel event.  Then the wave
/// loop continues — the run completes with whatever the other specialists
/// found.
pub(in super::super) fn fold_hunt_degraded(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    d: &HuntDispatch,
    err: &str,
    requeue_enabled: bool,
) {
    // Health signal: every degraded specialist is counted for the report's Run
    // Health section, whether or not it is retried.
    checkpoint.run_health.degraded_specialists =
        checkpoint.run_health.degraded_specialists.saturating_add(1);

    // Opt-in one-shot requeue (DYSON_SEC_REQUEUE_SHALLOW): leave the batch
    // PENDING so the wave loop re-dispatches this class exactly once more. The
    // retry is bounded both per-class (`requeued_classes`) and globally
    // (`HUNT_SPECIALIST_BACKSTOP`), so a persistently-failing class still
    // terminates the wave loop after its second attempt falls through below.
    let retry =
        d.is_class && requeue_enabled && !checkpoint.run_health.requeued_classes.contains(&d.label);
    if retry {
        checkpoint.run_health.requeued_classes.push(d.label.clone());
        checkpoint.coverage_gaps.push(CoverageGap {
            area: d.label.clone(),
            reason: format!("hunt specialist did not complete; retrying once: {err}"),
            risk: "unknown".into(),
        });
        aggregate.checkpoints.push(CheckpointEvent {
            message: format!(
                "security_engineer: hunt: class {} degraded (retrying once)",
                d.label
            ),
            progress: Some(0.35),
        });
        return;
    }

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
pub(in super::super) fn fold_hunt_result(
    checkpoint: &mut SecurityCheckpoint,
    aggregate: &mut ToolOutput,
    d: &HuntDispatch,
    raw: String,
    stage_out: ToolOutput,
) {
    merge_stage_tool_output(aggregate, stage_out);
    // A malformed response is degraded coverage, not a clean empty result.
    // Route it through the same retry/gap bookkeeping as a transport failure
    // so the class can never be reported as checked-and-cleared.
    let hunt: HuntStageOutput = match parse_stage_json(&raw) {
        Ok(hunt) => hunt,
        Err(e) => {
            fold_hunt_degraded(
                checkpoint,
                aggregate,
                d,
                &format!("output was not parseable JSON: {e}"),
                requeue_shallow_enabled(),
            );
            return;
        }
    };
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
pub(in super::super) fn pending_tasks_for_class(
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

pub(in super::super) fn complete_tasks(
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

pub(in super::super) fn normalize_task_ids(tasks: &mut [SecurityTask], prefix: &str) {
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.id.trim().is_empty() {
            task.id = format!("{prefix}-{:03}", idx + 1);
        }
    }
}
