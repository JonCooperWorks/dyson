// ===========================================================================
// Markdown rendering for the final report + the deterministic fallback that
// reconstructs a report from the checkpoint when the LLM repair path fails.
//
// Also holds the small inline helpers that classify which findings should be
// reported (confirmed + complete + not a no-vulnerability note) and the
// dedupe-by-root-cause grouping the LLM is allowed to skip.
// ===========================================================================

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
        "- Vulnerability classes hunted: {}\n\n",
        report
            .class_coverage
            .iter()
            .filter(|class| class.hunted)
            .count()
    ));

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
            if !entry.summary.is_empty() {
                out.push_str(&format!(" - {}", clean_inline(&entry.summary)));
            }
            out.push('\n');
        }
    }

    out
}

pub(super) fn render_finding_markdown(
    out: &mut String,
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
    finding: &SecurityFinding,
) {
    out.push_str(&format!("### {}: {}\n\n", finding.id, finding.title));
    out.push_str(&format!("- Severity: `{}`\n", finding.severity));
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

    if let Some(group) = report
        .dedupe_groups
        .iter()
        .find(|group| group.finding_ids.iter().any(|id| id == &finding.id))
    {
        out.push_str(&format!("- Dedupe group: `{}`\n", group.id));
    }

    append_list(out, "Affected paths", &finding.affected_paths);
    append_list(out, "Evidence", &finding.evidence);
    out.push('\n');
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
    filtered
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
        coverage: checkpoint.coverage_gaps.clone(),
        gaps: checkpoint.coverage_gaps.clone(),
        dedupe_groups: dedupe_findings(&findings),
        trace_evidence,
        stage_history: checkpoint.stage_history.clone(),
        class_coverage: checkpoint.class_coverage.clone(),
    }
}
