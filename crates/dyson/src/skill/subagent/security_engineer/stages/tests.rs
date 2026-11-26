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

fn finding_with_id(id: &str) -> SecurityFinding {
    SecurityFinding {
        id: id.into(),
        ..Default::default()
    }
}

#[test]
fn shard_by_findings_single_shard_at_or_below_threshold() {
    let c = cp();
    let findings: Vec<SecurityFinding> = (0..STAGE_SHARD_SIZE)
        .map(|i| finding_with_id(&format!("f{i}")))
        .collect();
    let shards = shard_by_findings(&c, &findings);
    assert_eq!(
        shards.len(),
        1,
        "a finding set at the threshold must stay a single child"
    );
    assert_eq!(shards[0].findings_so_far.len(), STAGE_SHARD_SIZE);
}

#[test]
fn shard_by_findings_fans_out_large_sets_covering_each_finding_once() {
    let c = cp();
    let n = STAGE_SHARD_SIZE * 2 + 3;
    let findings: Vec<SecurityFinding> =
        (0..n).map(|i| finding_with_id(&format!("f{i}"))).collect();
    let shards = shard_by_findings(&c, &findings);
    assert!(
        shards.len() >= 3,
        "a set larger than 2x the shard size must fan out, got {} shards",
        shards.len()
    );
    assert!(
        shards
            .iter()
            .all(|s| s.findings_so_far.len() <= STAGE_SHARD_SIZE),
        "no shard may exceed STAGE_SHARD_SIZE findings"
    );
    let total: usize = shards.iter().map(|s| s.findings_so_far.len()).sum();
    assert_eq!(total, n, "every finding must appear in exactly one shard");
    // Partition, not duplication: the union of shard finding ids equals the input.
    let mut ids: Vec<String> = shards
        .iter()
        .flat_map(|s| s.findings_so_far.iter().map(|f| f.id.clone()))
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        n,
        "shards must partition the findings without overlap"
    );
}

#[test]
fn missing_trace_result_is_preserved_as_unknown() {
    let mut c = cp();
    let finding = finding_with_id("finding-001");
    c.findings_so_far.push(finding.clone());
    complete_trace_results(&mut c, &[finding]).expect("unknown trace should be synthesized");
    assert_eq!(c.trace_results_so_far.len(), 1);
    assert_eq!(c.trace_results_so_far[0].reachable, None);
    assert!(
        c.run_health
            .incomplete_results
            .contains(&"trace:finding-001".to_string())
    );
    assert!(
        c.coverage_gaps
            .iter()
            .any(|gap| gap.area == "Trace for finding-001")
    );
}

#[test]
fn evidence_backed_unreachable_trace_remains_false() {
    let mut c = cp();
    let finding = finding_with_id("finding-001");
    c.findings_so_far.push(finding.clone());
    c.trace_results_so_far
        .push(super::super::types::TraceResult {
            finding_id: finding.id.clone(),
            reachable: Some(false),
            severity_effect: "downgrades".into(),
            evidence: vec!["route is not registered".into()],
            consumer_paths: Vec::new(),
        });
    complete_trace_results(&mut c, &[finding]).expect("complete trace should pass");
    assert_eq!(c.trace_results_so_far[0].reachable, Some(false));
    assert!(c.run_health.incomplete_results.is_empty());
}

#[test]
fn missing_judgment_is_preserved_as_unknown() {
    let mut c = cp();
    let finding = finding_with_id("finding-001");
    c.findings_so_far.push(finding.clone());
    complete_judgment_results(&mut c, &[finding]).expect("unknown judgment should be synthesized");
    assert_eq!(c.judgment_results.len(), 1);
    assert_eq!(c.judgment_results[0].reachable_in_prod, None);
    assert!(
        c.run_health
            .incomplete_results
            .contains(&"judgment:finding-001".to_string())
    );
}

#[test]
fn feedback_queues_only_explicit_consumer_paths() {
    let mut c = cp();
    let mut finding = finding_with_id("finding-001");
    finding.vulnerability_class = "auth_authorization".into();
    finding.affected_paths = vec!["src/vulnerable_sink.rs:9".into()];
    c.findings_so_far.push(finding);
    c.trace_results_so_far
        .push(super::super::types::TraceResult {
            finding_id: "finding-001".into(),
            reachable: Some(true),
            severity_effect: "keeps".into(),
            evidence: vec!["resolved caller to sink".into()],
            consumer_paths: vec!["src/shipped_route.rs:42".into()],
        });

    run_feedback_stage(&mut c);

    assert_eq!(c.pending_tasks.len(), 1);
    assert_eq!(c.pending_tasks[0].scope_hint, "src/shipped_route.rs:42");
    assert_ne!(c.pending_tasks[0].scope_hint, "src/vulnerable_sink.rs:9");
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
        SecurityHarnessStage::Judgment,
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
        SecurityHarnessStage::Judgment,
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
fn fold_hunt_result_with_prose_records_degraded_coverage() {
    // Malformed output remains non-fatal, but must not masquerade as a
    // successful empty hunt or a checked-and-cleared class.
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
    // The failed batch is closed to prevent an infinite redispatch loop,
    // while its degraded state remains explicit.
    assert!(c.completed_tasks.iter().any(|t| t.id == "t1"));
    assert!(c.pending_tasks.iter().any(|t| t.id == "t2"));
    assert_eq!(c.run_health.degraded_specialists, 1);
    assert!(c.coverage_gaps.iter().any(|gap| {
        gap.area == "auth_authorization" && gap.reason.contains("not parseable JSON")
    }));
    assert!(
        !aggregate
            .checkpoints
            .iter()
            .any(|event| event.message.contains("auth_authorization cleared"))
    );
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
    let raw =
        String::from(r#"{"completed_task_ids":[],"findings":[],"gaps":[],"follow_up_tasks":[]}"#);
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
    fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "upstream stalled", false);
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
    fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "timed out", false);
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
fn fold_hunt_degraded_counts_every_degraded_specialist() {
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
    fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "stalled", false);
    assert_eq!(
        c.run_health.degraded_specialists, 1,
        "a degraded specialist must be counted for the Run Health section"
    );
}

#[test]
fn fold_hunt_degraded_requeues_a_class_once_then_completes_it() {
    // With requeue enabled, the first degradation leaves the batch PENDING
    // (so the wave loop re-dispatches it) and records the class as retried;
    // a second degradation falls through to mark the batch complete, so the
    // wave loop is guaranteed to terminate.
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

    // First failure with requeue on: batch stays pending, class recorded.
    fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "stalled", true);
    assert!(
        c.pending_tasks.iter().any(|t| t.id == "t1"),
        "requeue must leave the batch pending for a second attempt"
    );
    assert!(c.completed_tasks.is_empty(), "nothing completed on requeue");
    assert_eq!(c.run_health.requeued_classes, vec!["auth_authorization"]);

    // Second failure (still requeue on): already retried → falls through to
    // complete, terminating the wave loop.
    fold_hunt_degraded(&mut c, &mut aggregate, &dispatch, "stalled again", true);
    assert!(
        c.completed_tasks.iter().any(|t| t.id == "t1"),
        "the second degradation must mark the batch complete (bounded retry)"
    );
    assert_eq!(
        c.run_health.requeued_classes.len(),
        1,
        "a class is retried at most once"
    );
    assert_eq!(
        c.run_health.degraded_specialists, 2,
        "both degraded attempts are counted"
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
