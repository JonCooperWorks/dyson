//! Markdown rendering for the final report + the deterministic fallback that
//! reconstructs a report from the checkpoint when the LLM repair path fails.
//!
//! Also holds the small inline helpers that classify which findings should be
//! reported (confirmed + complete + not a no-vulnerability note) and the
//! dedupe-by-root-cause grouping the LLM is allowed to skip.

use std::collections::{BTreeMap, BTreeSet};

use super::SECURITY_HARNESS_SCHEMA_VERSION;
use super::parse::{is_no_vulnerability_note, missing_finding_evidence_fields};
use super::types::{
    DedupeGroup, SecurityCheckpoint, SecurityFinding, SecurityHarnessReport, ValidationDecisionKind,
};

pub(super) fn render_report_markdown(
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Security Harness Report: {}\n\n",
        report.target.repo_path
    ));
    out.push_str(&format!("- Run ID: `{}`\n", report.run_id));
    out.push_str(&format!("- Target: `{}`\n", report.target.repo_path));
    if let Some(git_ref) = &report.target.git_ref {
        out.push_str(&format!("- Git ref: `{git_ref}`\n"));
    }
    out.push_str(&format!(
        "- Checkpoint: `{}`\n",
        checkpoint.checkpoint_path()
    ));
    out.push_str(&format!(
        "- Report schema: `{}`\n\n",
        checkpoint.report_validation_state.status
    ));

    out.push_str("## Scope\n\n");
    out.push_str(&plain_block(&report.scope));
    out.push('\n');

    let confirmed = report.findings.len();
    let rejected = report.rejected_candidates.len();
    let reachable = report
        .trace_evidence
        .iter()
        .filter(|trace| trace.reachable)
        .count();
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Findings: {}\n", report.findings.len()));
    out.push_str(&format!("- Confirmed findings: {confirmed}\n"));
    out.push_str(&format!("- Rejected candidates: {rejected}\n"));
    out.push_str(&format!(
        "- Dedupe groups: {}\n",
        report.dedupe_groups.len()
    ));
    out.push_str(&format!(
        "- Reachable traces: {reachable}/{}\n",
        report.trace_evidence.len()
    ));
    out.push_str(&format!("- Coverage gaps: {}\n\n", report.gaps.len()));
    out.push_str(&format!(
        "- Vulnerability classes considered: {}\n",
        report.class_coverage.len()
    ));
    out.push_str(&format!(
        "- Vulnerability classes hunted: {}\n",
        report
            .class_coverage
            .iter()
            .filter(|class| class.hunted)
            .count()
    ));
    out.push_str(&format!(
        "- Ledger: {} new this run, {} recurring\n\n",
        checkpoint.ledger_summary.new_findings, checkpoint.ledger_summary.recurring_findings
    ));

    render_run_health(&mut out, report, checkpoint);

    out.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        out.push_str("No confirmed findings were reported.\n\n");
    } else {
        for finding in &report.findings {
            render_finding_markdown(&mut out, report, checkpoint, finding);
        }
    }

    out.push_str("## Rejected Candidates\n\n");
    if report.rejected_candidates.is_empty() {
        out.push_str("No rejected candidates were recorded.\n\n");
    } else {
        for decision in &report.rejected_candidates {
            out.push_str(&format!(
                "### {}\n\n- Decision: `{}`\n- Evidence: {}\n\n",
                decision.finding_id,
                validation_decision_label(decision.decision),
                clean_inline(&decision.evidence)
            ));
        }
    }

    out.push_str("## Coverage And Gaps\n\n");
    if report.class_coverage.is_empty() {
        out.push_str("No vulnerability-class coverage accounting was recorded.\n\n");
    } else {
        out.push_str("### Vulnerability Classes\n\n");
        for class in &report.class_coverage {
            out.push_str(&format!(
                "- **{}** (`{}`): considered={} applicable={} hunted={} cleared={}",
                clean_inline(&class.class_name),
                clean_inline(&class.class_id),
                class.considered,
                class.applicable,
                class.hunted,
                class.checked_and_cleared
            ));
            if class.high_risk_follow_up {
                out.push_str(" follow_up=true");
            }
            if !class.skipped_reason.trim().is_empty() {
                out.push_str(&format!(" skipped={}", clean_inline(&class.skipped_reason)));
            }
            if !class.task_ids.is_empty() {
                out.push_str(&format!(" tasks={}", inline_code_list(&class.task_ids)));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("### Gaps\n\n");
    if report.gaps.is_empty() {
        out.push_str("No coverage gaps were recorded.\n\n");
    } else {
        for gap in &report.gaps {
            out.push_str(&format!(
                "- **{}** (`{}`): {}\n",
                clean_inline(&gap.area),
                if gap.risk.is_empty() {
                    "unknown"
                } else {
                    gap.risk.as_str()
                },
                clean_inline(&gap.reason)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Dedupe Groups\n\n");
    if report.dedupe_groups.is_empty() {
        out.push_str("No dedupe groups were recorded.\n\n");
    } else {
        for group in &report.dedupe_groups {
            out.push_str(&format!(
                "### {}\n\n- Primary finding: `{}`\n- Findings: {}\n- Root cause: {}\n",
                group.id,
                group.primary_finding_id,
                inline_code_list(&group.finding_ids),
                clean_inline(&group.root_cause)
            ));
            append_list(&mut out, "Affected paths", &group.affected_paths);
            out.push('\n');
        }
    }

    out.push_str("## Stage History\n\n");
    if report.stage_history.is_empty() {
        out.push_str("No stage history was recorded.\n");
    } else {
        for entry in &report.stage_history {
            out.push_str(&format!(
                "- `{}`: {} in {}s",
                entry.stage,
                entry.status,
                entry.finished_at.saturating_sub(entry.started_at)
            ));
            if !entry.model.trim().is_empty() {
                out.push_str(&format!(" on `{}`", clean_inline(&entry.model)));
            }
            if !entry.summary.is_empty() {
                out.push_str(&format!(" - {}", clean_inline(&entry.summary)));
            }
            out.push('\n');
        }
    }

    out
}

/// Render the Run Health section: shallow/degraded-coverage signals so an
/// operator can tell when a "clean" run actually had reduced coverage. Always
/// emitted (even on a healthy run) so its absence is never ambiguous.
pub(super) fn render_run_health(
    out: &mut String,
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
) {
    let health = &checkpoint.run_health;
    let hunted_classes = report
        .class_coverage
        .iter()
        .filter(|class| class.hunted)
        .count();
    let classes_with_findings: BTreeSet<&str> = report
        .findings
        .iter()
        .map(|finding| finding.vulnerability_class.as_str())
        .collect();
    let cleared = report
        .class_coverage
        .iter()
        .filter(|class| class.hunted && !classes_with_findings.contains(class.class_id.as_str()))
        .count();

    out.push_str("## Run Health\n\n");
    out.push_str(&format!(
        "- Degraded hunt specialists: {}\n",
        health.degraded_specialists
    ));
    out.push_str(&format!(
        "- Hunted classes cleared (no findings): {cleared} of {hunted_classes} hunted\n"
    ));
    if !health.requeued_classes.is_empty() {
        out.push_str(&format!(
            "- Retried classes (shallow-requeue): {}\n",
            inline_code_list(&health.requeued_classes)
        ));
    }
    if !health.fast_stages.is_empty() {
        out.push_str(&format!(
            "- Possibly-shallow stages (fast return): {}\n",
            inline_code_list(&health.fast_stages)
        ));
    }
    // The one signal worth shouting about: nothing found AND something broke.
    // A zero-finding run with degraded specialists is far likelier "we missed
    // it" than "the code is clean" — tell the operator to re-run.
    if report.findings.is_empty() && health.degraded_specialists > 0 {
        out.push_str(
            "- WARNING: zero confirmed findings with degraded specialists — coverage is \
             likely incomplete; re-run (optionally with DYSON_SEC_REQUEUE_SHALLOW=1).\n",
        );
    }
    out.push('\n');
}

pub(super) fn render_finding_markdown(
    out: &mut String,
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
    finding: &SecurityFinding,
) {
    out.push_str(&format!("### {}: {}\n\n", finding.id, finding.title));
    out.push_str(&format!("- Severity: `{}`\n", finding.severity));
    if let Some(entry) = checkpoint
        .ledger_summary
        .entries
        .iter()
        .find(|e| e.finding_id == finding.id && !e.finding_key.is_empty())
    {
        out.push_str(&format!("- Ledger key: `{}`", entry.finding_key));
        if entry.recurring {
            out.push_str(&format!(" (recurring, seen {}x)", entry.occurrences));
        } else {
            out.push_str(" (new this run)");
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "- Vulnerability class: `{}`\n",
        clean_inline(&finding.vulnerability_class)
    ));
    out.push_str(&format!(
        "- Trust boundary: {}\n",
        clean_inline(&finding.trust_boundary)
    ));
    out.push_str(&format!(
        "- Entry point: {}\n",
        clean_inline(&finding.entry_point)
    ));
    out.push_str(&format!(
        "- Sink/security decision: {}\n",
        clean_inline(&finding.sink_or_decision)
    ));
    if !finding.reachability.is_empty() {
        out.push_str(&format!(
            "- Reachability: `{}`\n",
            clean_inline(&finding.reachability)
        ));
    }
    out.push_str(&format!(
        "- Root cause: {}\n",
        clean_inline(&finding.root_cause)
    ));
    if !finding.tenant_or_instance_impact.trim().is_empty() {
        out.push_str(&format!(
            "- Tenant/instance impact: {}\n",
            clean_inline(&finding.tenant_or_instance_impact)
        ));
    }
    if !finding.severity_rationale.trim().is_empty() {
        out.push_str(&format!(
            "- Severity rationale: {}\n",
            clean_inline(&finding.severity_rationale)
        ));
    }
    if !finding.fix_recommendation.trim().is_empty() {
        out.push_str(&format!(
            "- Fix recommendation: {}\n",
            clean_inline(&finding.fix_recommendation)
        ));
    }

    if let Some(decision) = checkpoint
        .validation_decisions_so_far
        .iter()
        .find(|d| d.finding_id == finding.id)
    {
        out.push_str(&format!(
            "- Validation: `{}`",
            validation_decision_label(decision.decision)
        ));
        if let Some(severity) = &decision.severity {
            out.push_str(&format!(" as `{severity}`"));
        }
        out.push_str(&format!(" - {}\n", clean_inline(&decision.evidence)));
    }

    if let Some(trace) = report
        .trace_evidence
        .iter()
        .find(|trace| trace.finding_id == finding.id)
    {
        out.push_str(&format!(
            "- Trace: `{}`",
            if trace.reachable {
                "reachable"
            } else {
                "not reachable"
            }
        ));
        if !trace.severity_effect.is_empty() {
            out.push_str(&format!("; severity `{}`", trace.severity_effect));
        }
        out.push('\n');
        append_list(out, "Trace evidence", &trace.evidence);
    }

    if let Some(judgment) = checkpoint
        .judgment_results
        .iter()
        .find(|j| j.finding_id == finding.id)
    {
        out.push_str(&format!(
            "- Prod reachability: `{}`",
            if judgment.reachable_in_prod {
                "reachable"
            } else {
                "not reachable"
            }
        ));
        if !judgment.severity_effect.trim().is_empty() {
            out.push_str(&format!(
                "; severity `{}`",
                clean_inline(&judgment.severity_effect)
            ));
        }
        out.push('\n');
        if !judgment.rationale.trim().is_empty() {
            out.push_str(&format!("  - {}\n", clean_inline(&judgment.rationale)));
        }
    }

    if let Some(group) = report
        .dedupe_groups
        .iter()
        .find(|group| group.finding_ids.iter().any(|id| id == &finding.id))
    {
        out.push_str(&format!("- Dedupe group: `{}`\n", group.id));
    }

    append_list(out, "Affected paths", &finding.affected_paths);
    append_list(out, "Evidence", &finding.evidence);
    append_suggested_patch(out, &finding.suggested_patch);
    out.push('\n');
}

/// Render the optional fix diff as a fenced ```diff block. Suggestion only —
/// the harness never applies it; a human reviews and applies it. The patch is
/// emitted verbatim (not run through `clean_inline`, which collapses the
/// whitespace a diff depends on). Empty patches render nothing.
pub(super) fn append_suggested_patch(out: &mut String, patch: &str) {
    let patch = patch.trim_end();
    if patch.trim().is_empty() {
        return;
    }
    // A diff line can itself contain a ``` run (e.g. a removed Markdown fence),
    // which would close our block early. Make the fence longer than the longest
    // consecutive backtick run anywhere in the patch, and at least 3.
    let mut longest_run = 0usize;
    let mut current = 0usize;
    for ch in patch.chars() {
        if ch == '`' {
            current += 1;
            longest_run = longest_run.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat((longest_run + 1).max(3));
    out.push_str("\nSuggested patch (review before applying):\n");
    out.push_str(&format!("{fence}diff\n{patch}\n{fence}\n"));
}

pub(super) fn append_list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n{title}:\n"));
    for item in items {
        out.push_str(&format!("- {}\n", clean_inline(item)));
    }
}

pub(super) fn plain_block(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "No explicit scope was recorded.\n".into();
    }
    trimmed
        .lines()
        .map(|line| format!("> {}\n", line.trim()))
        .collect()
}

pub(super) fn clean_inline(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

pub(super) fn inline_code_list(values: &[String]) -> String {
    if values.is_empty() {
        return "`none`".into();
    }
    values
        .iter()
        .map(|value| format!("`{}`", value))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn validation_decision_label(decision: ValidationDecisionKind) -> &'static str {
    match decision {
        ValidationDecisionKind::Confirmed => "confirmed",
        ValidationDecisionKind::Rejected => "rejected",
        ValidationDecisionKind::NeedsMoreEvidence => "needs_more_evidence",
        ValidationDecisionKind::Downgrade => "downgrade",
    }
}

pub(crate) fn reportable_confirmed_findings(
    checkpoint: &SecurityCheckpoint,
) -> Vec<&SecurityFinding> {
    let confirmed_ids: BTreeSet<&str> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|decision| decision.decision == ValidationDecisionKind::Confirmed)
        .map(|decision| decision.finding_id.as_str())
        .collect();
    checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| confirmed_ids.contains(finding.id.as_str()))
        .filter(|finding| missing_finding_evidence_fields(finding).is_empty())
        .filter(|finding| !is_no_vulnerability_note(finding))
        .collect()
}

pub(super) fn reportable_finding_ids(checkpoint: &SecurityCheckpoint) -> BTreeSet<String> {
    reportable_confirmed_findings(checkpoint)
        .into_iter()
        .map(|finding| finding.id.clone())
        .collect()
}

pub(super) fn report_checkpoint_for_prompt(checkpoint: &SecurityCheckpoint) -> SecurityCheckpoint {
    let mut filtered = checkpoint.clone();
    let reportable_ids = reportable_finding_ids(checkpoint);
    filtered.findings_so_far = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| reportable_ids.contains(&finding.id))
        .cloned()
        .collect();
    filtered.dedupe_groups_so_far = dedupe_findings(&filtered.findings_so_far);
    filtered.trace_results_so_far = checkpoint
        .trace_results_so_far
        .iter()
        .filter(|trace| reportable_ids.contains(&trace.finding_id))
        .cloned()
        .collect();
    // Drop the orchestration-only task queues from the prompt. The report
    // contract is "use only findings, rejected candidates, class coverage,
    // gaps, dedupe groups, trace evidence, and stage history" — the per-task
    // hunt/gapfill queues are never read, but on a large run they balloon the
    // serialized checkpoint the report model has to ingest, which is what tips
    // a heavy run into a JSON-shape failure and the deterministic fallback.
    // Coverage is already conveyed by `class_coverage` + `stage_history`.
    filtered.completed_tasks = Vec::new();
    filtered.pending_tasks = Vec::new();
    filtered.gapfill_tasks = Vec::new();
    filtered
}

/// Cluster findings by stable fingerprint (see
/// [`super::ledger::finding_fingerprint`]) rather than by exact `root_cause`
/// string. Two hunters describing the same flaw in different words — same class,
/// files, and trust boundary — now collapse into one group, which exact-string
/// matching missed. The group's displayed `root_cause` is the primary finding's
/// (falling back to its title), preserving the human-readable label.
pub fn dedupe_findings(findings: &[SecurityFinding]) -> Vec<DedupeGroup> {
    // BTreeMap over the fingerprint string keeps group ordering + ids stable for
    // a given input (the stable-id contract downstream code relies on).
    let mut by_fp: BTreeMap<String, Vec<&SecurityFinding>> = BTreeMap::new();
    for finding in findings {
        by_fp
            .entry(super::ledger::finding_fingerprint(finding))
            .or_default()
            .push(finding);
    }
    by_fp
        .into_iter()
        .enumerate()
        .map(|(idx, (_fingerprint, group))| {
            let primary = group.first().copied();
            let root_cause = primary
                .map(|f| {
                    if f.root_cause.trim().is_empty() {
                        f.title.clone()
                    } else {
                        f.root_cause.clone()
                    }
                })
                .unwrap_or_default();
            let primary_id = primary.map(|f| f.id.clone()).unwrap_or_default();
            let mut affected = BTreeSet::new();
            for finding in &group {
                affected.extend(finding.affected_paths.iter().cloned());
            }
            DedupeGroup {
                id: format!("dedupe-{:03}", idx + 1),
                root_cause,
                primary_finding_id: primary_id,
                finding_ids: group.iter().map(|f| f.id.clone()).collect(),
                affected_paths: affected.into_iter().collect(),
            }
        })
        .collect()
}

pub(crate) fn report_from_checkpoint(checkpoint: &SecurityCheckpoint) -> SecurityHarnessReport {
    let reportable_ids = reportable_finding_ids(checkpoint);
    let findings = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| reportable_ids.contains(&finding.id))
        .cloned()
        .collect::<Vec<_>>();
    let rejected_candidates = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Rejected)
        .cloned()
        .collect();
    let trace_evidence = checkpoint
        .trace_results_so_far
        .iter()
        .filter(|trace| reportable_ids.contains(&trace.finding_id))
        .cloned()
        .collect();
    SecurityHarnessReport {
        schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
        run_id: checkpoint.run_id.clone(),
        target: checkpoint.target.clone(),
        scope: checkpoint.scope.clone(),
        findings: findings.clone(),
        rejected_candidates,
        gaps: checkpoint.coverage_gaps.clone(),
        dedupe_groups: dedupe_findings(&findings),
        trace_evidence,
        stage_history: checkpoint.stage_history.clone(),
        class_coverage: checkpoint.class_coverage.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{
        LedgerSummary, LedgerSummaryEntry, ModelMetadata, TargetRef, ValidationDecision,
        VulnerabilityClassCoverage,
    };
    use super::*;

    fn cp_with_target(repo: &str) -> SecurityCheckpoint {
        SecurityCheckpoint::new(
            "run-r".into(),
            TargetRef {
                repo_path: repo.into(),
                git_ref: None,
            },
            "scope text".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        )
    }

    fn finding(id: &str, root_cause: &str) -> SecurityFinding {
        SecurityFinding {
            id: id.into(),
            title: format!("title for {id}"),
            severity: "medium".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "boundary".into(),
            entry_point: "src/lib.rs:1".into(),
            sink_or_decision: "decision".into(),
            root_cause: root_cause.into(),
            affected_paths: vec!["src/lib.rs:1".into()],
            evidence: vec!["evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "impact".into(),
            severity_rationale: "rationale".into(),
            fix_recommendation: "fix".into(),
            suggested_patch: String::new(),
        }
    }

    fn report_with(
        checkpoint: &SecurityCheckpoint,
        findings: Vec<SecurityFinding>,
    ) -> SecurityHarnessReport {
        SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: checkpoint.run_id.clone(),
            target: checkpoint.target.clone(),
            scope: checkpoint.scope.clone(),
            findings,
            rejected_candidates: vec![],
            gaps: vec![],
            dedupe_groups: vec![],
            trace_evidence: vec![],
            stage_history: vec![],
            class_coverage: vec![VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                class_name: "Authentication and authorization".into(),
                considered: true,
                applicable: true,
                hunted: true,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn render_report_markdown_includes_title_run_id_target_and_scope() {
        let cp = cp_with_target("/repo");
        let report = report_with(&cp, vec![finding("finding-001", "rc")]);
        let md = render_report_markdown(&report, &cp);
        assert!(md.contains("# Security Harness Report"), "missing title");
        assert!(md.contains(&cp.run_id), "missing run_id");
        assert!(md.contains("/repo"), "missing target repo_path");
        assert!(md.contains("scope text"), "missing scope content");
    }

    #[test]
    fn render_report_markdown_with_no_findings_emits_no_confirmed_findings_section() {
        let cp = cp_with_target("/repo");
        let report = report_with(&cp, vec![]);
        let md = render_report_markdown(&report, &cp);
        assert!(!md.is_empty(), "markdown should never be empty");
        assert!(
            md.contains("No confirmed findings were reported."),
            "zero-findings report should include the empty-findings sentinel"
        );
    }

    #[test]
    fn render_finding_markdown_emits_non_empty_output_for_minimal_finding() {
        // All optional fields empty: ensure no panic / no unreachable!.
        let cp = cp_with_target("/repo");
        let report = report_with(&cp, vec![]);
        let f = SecurityFinding {
            id: "finding-min".into(),
            title: "minimal".into(),
            severity: String::new(),
            vulnerability_class: String::new(),
            trust_boundary: String::new(),
            entry_point: String::new(),
            sink_or_decision: String::new(),
            root_cause: String::new(),
            affected_paths: vec![],
            evidence: vec![],
            reachability: String::new(),
            tenant_or_instance_impact: String::new(),
            severity_rationale: String::new(),
            fix_recommendation: String::new(),
            suggested_patch: String::new(),
        };
        let mut buf = String::new();
        render_finding_markdown(&mut buf, &report, &cp, &f);
        assert!(
            !buf.is_empty(),
            "render_finding_markdown must produce output even for a stripped-down finding"
        );
        assert!(
            buf.contains("finding-min"),
            "rendered output should mention the finding id"
        );
    }

    #[test]
    fn security_finding_round_trips_suggested_patch() {
        let mut f = finding("finding-patch", "rc");
        f.suggested_patch = "--- a/x.rs\n+++ b/x.rs\n@@\n-bad\n+good\n".into();
        let json = serde_json::to_string(&f).expect("serialize");
        let back: SecurityFinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(f, back, "suggested_patch must survive a JSON round-trip");
        // And a finding emitted without the field still parses (back-compat).
        let legacy = r#"{"id":"x","title":"t","root_cause":"rc"}"#;
        let parsed: SecurityFinding = serde_json::from_str(legacy).expect("legacy parse");
        assert!(
            parsed.suggested_patch.is_empty(),
            "absent suggested_patch must default to empty"
        );
    }

    #[test]
    fn append_suggested_patch_emits_diff_fence_only_when_present() {
        let mut empty = String::new();
        append_suggested_patch(&mut empty, "   ");
        assert!(empty.is_empty(), "blank patch renders nothing");

        let mut out = String::new();
        append_suggested_patch(&mut out, "--- a/x\n+++ b/x\n@@\n-a\n+b");
        assert!(out.contains("```diff\n"), "should open a diff fence: {out}");
        assert!(
            out.trim_end().ends_with("```"),
            "should close the fence: {out}"
        );
        assert!(out.contains("+b"), "patch body must be preserved verbatim");
        assert!(
            out.contains("Suggested patch (review before applying)"),
            "should label the block as a non-applied suggestion"
        );
    }

    #[test]
    fn append_suggested_patch_escalates_fence_past_inner_backticks() {
        // A diff that itself contains a ``` line must not close the outer fence
        // early — the outer fence grows to outlast the inner run.
        let mut out = String::new();
        append_suggested_patch(&mut out, "--- a/md\n+++ b/md\n@@\n-```js\n+```ts\n");
        assert!(
            out.contains("````diff\n"),
            "outer fence should be at least 4 backticks: {out}"
        );
        assert!(
            out.trim_end().ends_with("````"),
            "closing fence should match the escalated opener: {out}"
        );
    }

    #[test]
    fn run_health_section_surfaces_degraded_fast_and_requeued_signals() {
        let mut cp = cp_with_target("/repo");
        cp.run_health.degraded_specialists = 2;
        cp.run_health.fast_stages = vec!["recon:1s".into()];
        cp.run_health.requeued_classes = vec!["auth_authorization".into()];
        // Zero confirmed findings + degraded specialists → the loud warning.
        let report = report_with(&cp, vec![]);
        let md = render_report_markdown(&report, &cp);
        assert!(md.contains("## Run Health"), "section must always render");
        assert!(md.contains("Degraded hunt specialists: 2"));
        assert!(md.contains("recon:1s"), "fast stages must be listed");
        assert!(
            md.contains("auth_authorization"),
            "retried classes must be listed"
        );
        assert!(
            md.contains("WARNING"),
            "zero findings with degraded specialists must warn to re-run"
        );
    }

    #[test]
    fn run_health_section_omits_warning_when_findings_present() {
        let mut cp = cp_with_target("/repo");
        cp.run_health.degraded_specialists = 1;
        // A confirmed finding in the only hunted class → not a shallow run.
        let report = report_with(&cp, vec![finding("finding-001", "rc")]);
        let md = render_report_markdown(&report, &cp);
        assert!(md.contains("## Run Health"));
        assert!(
            !md.contains("WARNING"),
            "findings present means the run is not shallow — no warning"
        );
    }

    #[test]
    fn report_renders_prod_reachability_verdict_per_finding() {
        use super::super::types::JudgmentResult;
        let mut cp = cp_with_target("/repo");
        cp.judgment_results.push(JudgmentResult {
            finding_id: "finding-001".into(),
            reachable_in_prod: false,
            rationale: "route not mounted in deploy/compose.prod.yaml".into(),
            severity_effect: "downgrade to low".into(),
        });
        let report = report_with(&cp, vec![finding("finding-001", "rc")]);
        let md = render_report_markdown(&report, &cp);
        assert!(
            md.contains("Prod reachability: `not reachable`"),
            "a not-reachable verdict must render: {md}"
        );
        assert!(
            md.contains("downgrade to low"),
            "severity effect must render"
        );
        assert!(
            md.contains("route not mounted"),
            "rationale must render under the verdict"
        );
    }

    #[test]
    fn report_renders_ledger_summary_and_per_finding_key() {
        let mut cp = cp_with_target("/repo");
        cp.ledger_summary = LedgerSummary {
            new_findings: 1,
            recurring_findings: 2,
            entries: vec![LedgerSummaryEntry {
                finding_id: "finding-001".into(),
                finding_key: "DYS-ABCD1234".into(),
                recurring: true,
                occurrences: 3,
            }],
        };
        let report = report_with(&cp, vec![finding("finding-001", "rc")]);
        let md = render_report_markdown(&report, &cp);
        assert!(
            md.contains("Ledger: 1 new this run, 2 recurring"),
            "summary must show new/recurring counts"
        );
        assert!(
            md.contains("Ledger key: `DYS-ABCD1234`"),
            "finding must show its stable key"
        );
        assert!(
            md.contains("(recurring, seen 3x)"),
            "recurring findings must show the occurrence count"
        );
    }

    #[test]
    fn dedupe_findings_clusters_by_fingerprint_across_phrasing() {
        // a and b share class + file but use DIFFERENT root_cause wording — the
        // old exact-string match split them; fingerprint clustering collapses
        // them. c is the same class in a DIFFERENT file, so it stays separate.
        let a = finding("a", "the handler skips the owner check");
        let b = finding("b", "missing owner-scoped authorization permits IDOR");
        let mut c = finding("c", "the handler skips the owner check");
        c.affected_paths = vec!["src/other.rs:9".into()];
        c.entry_point = "src/other.rs:9".into();

        let groups = dedupe_findings(&[a, b, c]);
        assert_eq!(
            groups.len(),
            2,
            "same file collapses regardless of phrasing; a different file splits"
        );
        let merged = groups
            .iter()
            .find(|g| g.finding_ids.len() == 2)
            .expect("a and b must share a group");
        assert!(
            merged.finding_ids.contains(&"a".to_string())
                && merged.finding_ids.contains(&"b".to_string()),
            "the two same-file findings must cluster together: {:?}",
            merged.finding_ids
        );
    }

    #[test]
    fn dedupe_findings_falls_back_to_title_when_root_cause_empty() {
        let f = finding("only", "");
        let groups = dedupe_findings(std::slice::from_ref(&f));
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].root_cause, f.title,
            "empty root_cause should fall back to the title"
        );
    }

    #[test]
    fn dedupe_findings_produces_stable_group_ids() {
        let findings = vec![finding("a", "X"), finding("b", "Y")];
        let first = dedupe_findings(&findings);
        let second = dedupe_findings(&findings);
        let first_ids: Vec<&str> = first.iter().map(|g| g.id.as_str()).collect();
        let second_ids: Vec<&str> = second.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(
            first_ids, second_ids,
            "same input should produce identical dedupe group ids"
        );
    }

    #[test]
    fn reportable_confirmed_findings_includes_confirmed_complete_findings() {
        let mut cp = cp_with_target("/repo");
        cp.findings_so_far.push(finding("finding-001", "rc"));
        cp.validation_decisions_so_far.push(ValidationDecision {
            finding_id: "finding-001".into(),
            decision: ValidationDecisionKind::Confirmed,
            evidence: "ok".into(),
            severity: None,
        });
        let result = reportable_confirmed_findings(&cp);
        assert_eq!(
            result.len(),
            1,
            "a confirmed and complete finding should appear in reportable list"
        );
        assert_eq!(result[0].id, "finding-001");
    }

    #[test]
    fn report_checkpoint_for_prompt_drops_task_queues_but_keeps_reportable_facts() {
        use super::super::types::{SecurityTask, TaskStatus};
        let mut cp = cp_with_target("/repo");
        cp.findings_so_far.push(finding("finding-001", "rc"));
        cp.validation_decisions_so_far.push(ValidationDecision {
            finding_id: "finding-001".into(),
            decision: ValidationDecisionKind::Confirmed,
            evidence: "ok".into(),
            severity: None,
        });
        cp.class_coverage.push(VulnerabilityClassCoverage {
            class_id: "auth_authorization".into(),
            class_name: "Authentication and authorization".into(),
            considered: true,
            applicable: true,
            hunted: true,
            ..Default::default()
        });
        let task = |id: &str| SecurityTask {
            id: id.into(),
            attack_class: "auth_authorization".into(),
            scope_hint: "scope".into(),
            status: TaskStatus::Completed,
            rationale: "r".into(),
        };
        cp.completed_tasks.push(task("t-done"));
        cp.pending_tasks.push(task("t-pending"));
        cp.gapfill_tasks.push(task("t-gap"));

        let pruned = report_checkpoint_for_prompt(&cp);
        // Orchestration queues are dropped to shrink the report model's input.
        assert!(
            pruned.completed_tasks.is_empty(),
            "completed_tasks must be dropped from the report prompt"
        );
        assert!(
            pruned.pending_tasks.is_empty(),
            "pending_tasks must be dropped"
        );
        assert!(
            pruned.gapfill_tasks.is_empty(),
            "gapfill_tasks must be dropped"
        );
        // The facts the report contract relies on must survive the prune.
        assert_eq!(
            pruned.findings_so_far.len(),
            1,
            "reportable findings must survive the prune"
        );
        assert_eq!(pruned.findings_so_far[0].id, "finding-001");
        assert_eq!(
            pruned.class_coverage.len(),
            1,
            "class coverage must survive the prune"
        );
    }

    #[test]
    fn reportable_confirmed_findings_excludes_rejected_findings() {
        let mut cp = cp_with_target("/repo");
        cp.findings_so_far.push(finding("finding-001", "rc"));
        cp.validation_decisions_so_far.push(ValidationDecision {
            finding_id: "finding-001".into(),
            decision: ValidationDecisionKind::Rejected,
            evidence: "rejected".into(),
            severity: None,
        });
        assert!(
            reportable_confirmed_findings(&cp).is_empty(),
            "rejected findings should not be reportable"
        );
    }

    #[test]
    fn reportable_confirmed_findings_excludes_needs_more_evidence_decisions() {
        let mut cp = cp_with_target("/repo");
        cp.findings_so_far.push(finding("finding-001", "rc"));
        cp.validation_decisions_so_far.push(ValidationDecision {
            finding_id: "finding-001".into(),
            decision: ValidationDecisionKind::NeedsMoreEvidence,
            evidence: "tbd".into(),
            severity: None,
        });
        assert!(
            reportable_confirmed_findings(&cp).is_empty(),
            "NeedsMoreEvidence decisions should not graduate to reportable"
        );
    }

    #[test]
    fn reportable_confirmed_findings_excludes_findings_with_no_decision() {
        let mut cp = cp_with_target("/repo");
        cp.findings_so_far.push(finding("finding-001", "rc"));
        // No decision recorded.
        assert!(
            reportable_confirmed_findings(&cp).is_empty(),
            "findings with no validation decision should not be reportable"
        );
    }
}
