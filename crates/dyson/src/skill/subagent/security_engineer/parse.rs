//! JSON extraction + per-stage parsers, plus the report schema validators.
//!
//! LLM stage outputs are wrapped in prose, code fences, and echoes of the
//! prompt's checkpoint JSON.  `extract_json` walks brace-balanced candidates
//! and returns the LAST one that parses — this avoids blending the prompt's
//! own embedded JSON with the model's real output.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::SECURITY_HARNESS_SCHEMA_VERSION;
use super::report::clean_inline;
use super::taxonomy::canonical_vulnerability_class;
use super::types::{
    DedupeGroup, SecurityFinding, SecurityHarnessReport, ValidateStageOutput,
    ValidationDecisionKind,
};

pub(super) fn parse_stage_json<T: for<'de> Deserialize<'de>>(
    raw: &str,
) -> std::result::Result<T, String> {
    let value = parse_json_value(raw)?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}

pub(super) fn parse_json_value(raw: &str) -> std::result::Result<serde_json::Value, String> {
    let candidate =
        extract_json(raw).ok_or_else(|| "no JSON object found in stage output".to_string())?;
    serde_json::from_str(candidate).map_err(|e| {
        let preview: String = candidate.chars().take(120).collect();
        format!("invalid JSON: {e} (extracted prefix: {preview:?})")
    })
}

// Scan `raw` for top-level balanced `{...}` substrings and return the LAST
// one that parses as a JSON object. The stage subagents are instructed to
// emit a single JSON object, but their prompt already contains a fenced
// checkpoint JSON, and weaker models surround their real output with prose,
// echo prompt fragments, or wrap braces in markdown. A greedy first-`{` to
// last-`}` scan blends those into garbage that fails at the first unquoted
// "key", so we walk forward with brace-depth + string-escape awareness and
// validate each candidate before accepting it.
pub(super) fn extract_json(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let mut last_valid: Option<&str> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = scan_balanced_brace(bytes, i) {
                let candidate = raw[i..=end].trim();
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    last_valid = Some(candidate);
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    last_valid
}

// From `start` (index of `{`) walk forward and return the index of the
// matching closing `}`, respecting JSON string boundaries and `\\`/`\"`
// escapes. Returns None if the input is unbalanced. Operates on bytes; safe
// for UTF-8 input because every delimiter we care about is single-byte ASCII.
pub(super) fn scan_balanced_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn parse_report_output(raw: &str) -> std::result::Result<SecurityHarnessReport, String> {
    let value = parse_json_value(raw)?;
    validate_report_json(&value)
}

/// Parse the validator's output into a [`ValidateStageOutput`] WITHOUT running
/// the semantic gate.
///
/// "Shape" failures (no JSON at all, or `{"findings": [...]}` which violates
/// the validator's "decide only, don't emit new findings" contract) are not
/// the same as "semantic" failures (finding_id refs, missing evidence,
/// confirming a no-vulnerability note).  The caller decides whether to be
/// strict about each — production code is loose at shape, strict at semantic.
pub(crate) fn parse_validate_output_shape(
    raw: &str,
) -> std::result::Result<ValidateStageOutput, String> {
    let value = parse_json_value(raw)?;
    if value.get("findings").is_some() {
        return Err("validator output must not include new findings".into());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Apply the semantic gate to a parsed validator output.  Catches the
/// hallucination class: the model invents a finding_id that doesn't exist,
/// confirms a finding missing required evidence fields, or tries to confirm
/// a "no vulnerability" note as if it were a real finding.  Always strict —
/// these are quality-floor checks, not parse-fragility.
pub(crate) fn validate_decisions_semantic(
    parsed: &ValidateStageOutput,
    findings: &[SecurityFinding],
) -> std::result::Result<(), String> {
    let known: BTreeSet<&str> = findings.iter().map(|f| f.id.as_str()).collect();
    for decision in &parsed.decisions {
        if !known.contains(decision.finding_id.as_str()) {
            return Err(format!(
                "validator referenced unknown finding_id {}",
                decision.finding_id
            ));
        }
        if decision.decision == ValidationDecisionKind::Confirmed {
            if decision.evidence.trim().is_empty() {
                return Err(format!(
                    "validator confirmation for {} requires evidence",
                    decision.finding_id
                ));
            }
            if let Some(finding) = findings.iter().find(|f| f.id == decision.finding_id) {
                let missing = missing_finding_evidence_fields(finding);
                if !missing.is_empty() {
                    return Err(format!(
                        "validator cannot confirm {} without required finding fields: {}",
                        decision.finding_id,
                        missing.join(", ")
                    ));
                }
                if is_no_vulnerability_note(finding) {
                    return Err(format!(
                        "validator cannot confirm {} because it is a no-vulnerability verification note, not a reportable finding",
                        decision.finding_id
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Parse + semantic-validate in one shot.  Convenience for tests and direct
/// callers; stage runners split the two so they can be loose at shape and
/// strict at semantic.
#[allow(dead_code)] // used by tests/tests.rs which cargo check --lib can't see
pub(crate) fn parse_validate_output(
    raw: &str,
    findings: &[SecurityFinding],
) -> std::result::Result<ValidateStageOutput, String> {
    let parsed = parse_validate_output_shape(raw)?;
    validate_decisions_semantic(&parsed, findings)?;
    Ok(parsed)
}

pub fn validate_report_json(
    value: &serde_json::Value,
) -> std::result::Result<SecurityHarnessReport, String> {
    prevalidate_report_value(value)?;
    let report: SecurityHarnessReport =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    validate_report_struct(report)
}

pub(super) fn validate_report_struct(
    report: SecurityHarnessReport,
) -> std::result::Result<SecurityHarnessReport, String> {
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
    if report.class_coverage.is_empty() {
        return Err("class_coverage is required".into());
    }
    for finding in &report.findings {
        if finding.id.trim().is_empty()
            || finding.title.trim().is_empty()
            || finding.root_cause.trim().is_empty()
        {
            return Err("findings require id, title, and root_cause".into());
        }
        let missing = missing_finding_evidence_fields(finding);
        if !missing.is_empty() {
            return Err(format!(
                "finding {} missing required evidence fields: {}",
                finding.id,
                missing.join(", ")
            ));
        }
    }
    for (idx, group) in report.dedupe_groups.iter().enumerate() {
        if group.id.trim().is_empty()
            || group.primary_finding_id.trim().is_empty()
            || group.finding_ids.is_empty()
            || group.root_cause.trim().is_empty()
        {
            return Err(format!(
                "dedupe_groups[{idx}] {} requires id, primary_finding_id, finding_ids, and root_cause",
                describe_dedupe_group(group)
            ));
        }
    }
    Ok(report)
}

pub(super) fn prevalidate_report_value(
    value: &serde_json::Value,
) -> std::result::Result<(), String> {
    if let Some(findings) = value.get("findings").and_then(|v| v.as_array()) {
        for (idx, finding) in findings.iter().enumerate() {
            if missing_or_empty_string(finding.get("root_cause")) {
                return Err(format!(
                    "findings[{idx}] {} missing required field root_cause",
                    describe_value_item(finding, &["id", "title"])
                ));
            }
        }
    }
    if let Some(groups) = value.get("dedupe_groups").and_then(|v| v.as_array()) {
        for (idx, group) in groups.iter().enumerate() {
            if missing_or_empty_string(group.get("root_cause")) {
                return Err(format!(
                    "dedupe_groups[{idx}] {} missing required field root_cause",
                    describe_value_item(group, &["id", "primary_finding_id"])
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn missing_or_empty_string(value: Option<&serde_json::Value>) -> bool {
    match value.and_then(|v| v.as_str()) {
        Some(s) => s.trim().is_empty(),
        None => true,
    }
}

pub(super) fn describe_value_item(value: &serde_json::Value, keys: &[&str]) -> String {
    let details = keys
        .iter()
        .filter_map(|key| value.get(*key).and_then(|v| v.as_str()).map(|v| (*key, v)))
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| format!("{key}={}", clean_inline(value)))
        .collect::<Vec<_>>();
    if details.is_empty() {
        "(unidentified item)".into()
    } else {
        format!("({})", details.join(" "))
    }
}

pub(super) fn describe_dedupe_group(group: &DedupeGroup) -> String {
    let mut details = Vec::new();
    if !group.id.trim().is_empty() {
        details.push(format!("id={}", clean_inline(&group.id)));
    }
    if !group.primary_finding_id.trim().is_empty() {
        details.push(format!(
            "primary_finding_id={}",
            clean_inline(&group.primary_finding_id)
        ));
    }
    if details.is_empty() {
        "(unidentified item)".into()
    } else {
        format!("({})", details.join(" "))
    }
}

pub(super) fn missing_finding_evidence_fields(finding: &SecurityFinding) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if canonical_vulnerability_class(&finding.vulnerability_class).is_none() {
        missing.push("vulnerability_class");
    }
    if finding.trust_boundary.trim().is_empty() {
        missing.push("trust_boundary");
    }
    if finding.entry_point.trim().is_empty() {
        missing.push("entry_point");
    }
    if finding.sink_or_decision.trim().is_empty() {
        missing.push("sink_or_decision");
    }
    if finding.root_cause.trim().is_empty() {
        missing.push("root_cause");
    }
    if finding.evidence.is_empty() {
        missing.push("evidence");
    }
    if finding.severity_rationale.trim().is_empty() {
        missing.push("severity_rationale");
    }
    if finding.fix_recommendation.trim().is_empty() {
        missing.push("fix_recommendation");
    }
    missing
}

pub(super) fn is_no_vulnerability_note(finding: &SecurityFinding) -> bool {
    let title = finding.title.to_ascii_lowercase();
    let root_cause = finding.root_cause.to_ascii_lowercase();
    let rationale = finding.severity_rationale.to_ascii_lowercase();
    let haystack = format!("{title}\n{root_cause}\n{rationale}");

    title.trim_start().starts_with("n/a")
        || haystack.contains("no vulnerability found")
        || haystack.contains("no bypass found")
        || haystack.contains("verified secure")
        || haystack.contains("verified safe")
        || haystack.contains("checked and cleared")
        || (title.contains("verified") && root_cause.contains("no vulnerability"))
        || (title.contains("verified") && rationale.contains("no vulnerability"))
}

#[cfg(test)]
mod extract_json_tests {
    use super::super::types::{
        ReconStageOutput, ValidateStageOutput, ValidationDecision, ValidationDecisionKind,
    };
    use super::*;

    #[test]
    fn picks_last_balanced_object_when_preamble_has_braces() {
        // Regression: a TARS run on vllm failed with
        //   "recon failed: invalid JSON: key must be a string at line 1 column 2"
        // because the recon subagent's preamble described code like
        //   "data["shape"] = {...}" and the greedy first-`{` to last-`}` scan
        // glommed that into the real JSON output.
        let raw = "Looking at the request, the writer sees `{shape: [...], dtype: torch.float32}` \
                   and then I will emit:\n\
                   ```json\n\
                   {\"architecture_context\": \"vllm distributed\", \"tasks\": []}\n\
                   ```\n";
        let candidate = extract_json(raw).expect("balanced object should be found");
        let parsed: serde_json::Value = serde_json::from_str(candidate).unwrap();
        assert_eq!(parsed["architecture_context"], "vllm distributed");
    }

    #[test]
    fn picks_last_balanced_object_when_prompt_checkpoint_is_echoed() {
        // The recon subagent's stage_message contains the checkpoint JSON
        // already; weaker models echo it before adding their own object.
        // The extractor must return the LAST valid object, not the first.
        let raw = "Here is the parent checkpoint:\n\
                   ```json\n\
                   {\"schema_version\": 1, \"current_stage\": \"recon\"}\n\
                   ```\n\
                   And my recon output:\n\
                   ```json\n\
                   {\"architecture_context\": \"x\", \"tasks\": [{\"id\": \"hunt-001\"}]}\n\
                   ```\n";
        let candidate = extract_json(raw).expect("object should be found");
        let parsed: serde_json::Value = serde_json::from_str(candidate).unwrap();
        assert_eq!(parsed["tasks"][0]["id"], "hunt-001");
    }

    #[test]
    fn handles_braces_inside_json_strings() {
        // A scope/context field can legitimately contain `{` and `}` (e.g.
        // a code snippet copied into the user's task). Brace-counting must
        // ignore those when inside a JSON string.
        let raw = r#"{"scope": "context=Target: foo {bar} baz", "tasks": []}"#;
        let candidate = extract_json(raw).expect("object should be found");
        assert_eq!(candidate, raw);
    }

    #[test]
    fn handles_escaped_quotes_inside_strings() {
        let raw = r#"{"note": "he said \"hi\" and {}"}"#;
        let candidate = extract_json(raw).expect("object should be found");
        let parsed: serde_json::Value = serde_json::from_str(candidate).unwrap();
        assert_eq!(parsed["note"], "he said \"hi\" and {}");
    }

    #[test]
    fn skips_invalid_candidates_and_keeps_last_valid() {
        // JS-style unquoted keys must not be returned even if they look
        // balanced. The first object here is the kind of broken thing
        // weaker models emit when they slip into JavaScript mode.
        let raw = "{architecture_context: \"x\"}\n\n\
                   ```json\n\
                   {\"architecture_context\": \"y\", \"tasks\": []}\n\
                   ```\n";
        let candidate = extract_json(raw).expect("object should be found");
        let parsed: serde_json::Value = serde_json::from_str(candidate).unwrap();
        assert_eq!(parsed["architecture_context"], "y");
    }

    #[test]
    fn returns_none_when_no_balanced_object_parses() {
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("{architecture: still no quotes}").is_none());
        // Unbalanced — opens but never closes.
        assert!(extract_json("{ \"x\": 1 ").is_none());
    }

    #[test]
    fn accepts_plain_object_without_fences() {
        let raw = r#"{"x": 1, "y": [1, 2, 3]}"#;
        let candidate = extract_json(raw).expect("object should be found");
        assert_eq!(candidate, raw);
    }

    #[test]
    fn recon_stage_output_tolerates_missing_required_fields() {
        // Live-fire failure on deepseek-v4-pro: the recon subagent emits
        // valid JSON but omits `architecture_context`. The old struct
        // required every field; the new struct uses `#[serde(default)]`
        // at the container level so any field can be missing.
        let raw = r#"{"tasks": [{"id":"hunt-001","attack_class":"ssrf"}], "coverage_gaps": []}"#;
        let recon: ReconStageOutput = parse_stage_json(raw).expect("recon should parse");
        assert!(recon.architecture_context.is_empty());
        assert_eq!(recon.tasks.len(), 1);
    }

    #[test]
    fn recon_stage_output_tolerates_empty_object() {
        // The minimum case: a model returns `{}` and the harness should
        // still continue. The taxonomy fan-out will populate hunt tasks
        // for every class regardless of what recon said.
        let recon: ReconStageOutput = parse_stage_json("{}").expect("empty object should parse");
        assert!(recon.architecture_context.is_empty());
        assert!(recon.tasks.is_empty());
        assert!(recon.coverage_gaps.is_empty());
        assert!(recon.class_coverage.is_empty());
    }

    fn finding_complete(id: &str) -> SecurityFinding {
        SecurityFinding {
            id: id.into(),
            title: "title".into(),
            severity: "medium".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "boundary".into(),
            entry_point: "src/lib.rs:1".into(),
            sink_or_decision: "decision".into(),
            root_cause: "cause".into(),
            affected_paths: vec!["src/lib.rs".into()],
            evidence: vec!["evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "impact".into(),
            severity_rationale: "rationale".into(),
            fix_recommendation: "fix".into(),
        }
    }

    #[test]
    fn parse_validate_output_succeeds_with_valid_confirmed_decision() {
        let findings = vec![finding_complete("finding-001")];
        let raw = r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"reproduced via taint trace"}]}"#;
        let out = parse_validate_output(raw, &findings).expect("should parse");
        assert_eq!(out.decisions.len(), 1);
        assert_eq!(out.decisions[0].finding_id, "finding-001");
    }

    #[test]
    fn parse_validate_output_rejects_decision_for_unknown_finding() {
        let findings = vec![finding_complete("finding-001")];
        let raw = r#"{"decisions":[{"finding_id":"finding-999","decision":"confirmed","evidence":"ok"}]}"#;
        let err = parse_validate_output(raw, &findings).unwrap_err();
        assert!(
            err.contains("unknown finding_id"),
            "expected 'unknown finding_id' in error, got: {err}"
        );
    }

    #[test]
    fn parse_validate_output_rejects_confirmed_finding_missing_vulnerability_class() {
        let mut f = finding_complete("finding-001");
        f.vulnerability_class.clear();
        let findings = vec![f];
        let raw = r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
        let err = parse_validate_output(raw, &findings).unwrap_err();
        assert!(
            err.contains("vulnerability_class"),
            "error should mention vulnerability_class, got: {err}"
        );
    }

    #[test]
    fn parse_validate_output_rejects_confirmed_finding_missing_trust_boundary() {
        let mut f = finding_complete("finding-001");
        f.trust_boundary.clear();
        let findings = vec![f];
        let raw = r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
        let err = parse_validate_output(raw, &findings).unwrap_err();
        assert!(
            err.contains("trust_boundary"),
            "error should mention trust_boundary, got: {err}"
        );
    }

    #[test]
    fn parse_validate_output_rejects_output_with_new_findings_field() {
        let findings = vec![finding_complete("finding-001")];
        let raw = r#"{"findings":[{"id":"x"}],"decisions":[]}"#;
        let err = parse_validate_output(raw, &findings).unwrap_err();
        assert!(
            err.contains("must not include new findings"),
            "expected new-findings rejection, got: {err}"
        );
    }

    #[test]
    fn parse_validate_output_rejects_no_vulnerability_finding_being_confirmed() {
        let mut f = finding_complete("finding-001");
        f.title = "no vulnerability found here".into();
        f.root_cause = "no vulnerability found in the tested boundary".into();
        let findings = vec![f];
        let raw = r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
        let err = parse_validate_output(raw, &findings).unwrap_err();
        assert!(
            err.contains("no-vulnerability verification note"),
            "expected no-vulnerability rejection, got: {err}"
        );
    }

    #[test]
    fn validate_report_struct_rejects_dedupe_referencing_unknown_finding() {
        // Note: validate_report_struct itself doesn't cross-check
        // dedupe finding_ids vs report.findings.ids — it just enforces
        // structural completeness. Pin that contract here: an unknown
        // finding_id in a dedupe group is NOT rejected today.
        let report = SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: "run-1".into(),
            target: super::super::types::TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            scope: "scope".into(),
            findings: vec![],
            rejected_candidates: vec![],
            coverage: vec![],
            gaps: vec![],
            dedupe_groups: vec![DedupeGroup {
                id: "dedupe-001".into(),
                root_cause: "rc".into(),
                primary_finding_id: "finding-unknown".into(),
                finding_ids: vec!["finding-unknown".into()],
                affected_paths: vec![],
            }],
            trace_evidence: vec![],
            stage_history: vec![],
            class_coverage: vec![super::super::types::VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                ..Default::default()
            }],
        };
        // Today this passes — pin the behavior so a future tightening
        // surfaces here.
        validate_report_struct(report).expect(
            "validate_report_struct does not cross-check dedupe_groups.finding_ids; \
             if you tightened it, update this test",
        );
    }

    #[test]
    fn validate_report_struct_rejects_dedupe_with_empty_primary_finding_id() {
        let report = SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: "run-1".into(),
            target: super::super::types::TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            scope: "scope".into(),
            findings: vec![],
            rejected_candidates: vec![],
            coverage: vec![],
            gaps: vec![],
            dedupe_groups: vec![DedupeGroup {
                id: "dedupe-001".into(),
                root_cause: "rc".into(),
                primary_finding_id: String::new(),
                finding_ids: vec!["finding-001".into()],
                affected_paths: vec![],
            }],
            trace_evidence: vec![],
            stage_history: vec![],
            class_coverage: vec![super::super::types::VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                ..Default::default()
            }],
        };
        let err = validate_report_struct(report).unwrap_err();
        assert!(
            err.contains("dedupe_groups"),
            "error should mention dedupe_groups, got: {err}"
        );
        assert!(
            err.contains("primary_finding_id"),
            "error should mention primary_finding_id, got: {err}"
        );
    }

    #[test]
    fn validate_report_struct_does_not_flag_duplicate_finding_ids() {
        // Pin behavior: validate_report_struct does NOT cross-check that
        // finding ids are unique within findings[]. If that's tightened
        // later, update this test.
        let f = finding_complete("finding-001");
        let report = SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: "run-1".into(),
            target: super::super::types::TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            scope: "scope".into(),
            findings: vec![f.clone(), f],
            rejected_candidates: vec![],
            coverage: vec![],
            gaps: vec![],
            dedupe_groups: vec![],
            trace_evidence: vec![],
            stage_history: vec![],
            class_coverage: vec![super::super::types::VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                ..Default::default()
            }],
        };
        validate_report_struct(report).expect(
            "validate_report_struct does not de-duplicate finding ids today; \
             if you tightened it, update this test",
        );
    }

    #[test]
    fn validate_report_struct_rejects_finding_missing_vulnerability_class() {
        let mut f = finding_complete("finding-001");
        f.vulnerability_class.clear();
        let report = SecurityHarnessReport {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            run_id: "run-1".into(),
            target: super::super::types::TargetRef {
                repo_path: "/repo".into(),
                git_ref: None,
            },
            scope: "scope".into(),
            findings: vec![f],
            rejected_candidates: vec![],
            coverage: vec![],
            gaps: vec![],
            dedupe_groups: vec![],
            trace_evidence: vec![],
            stage_history: vec![],
            class_coverage: vec![super::super::types::VulnerabilityClassCoverage {
                class_id: "auth_authorization".into(),
                ..Default::default()
            }],
        };
        let err = validate_report_struct(report).unwrap_err();
        assert!(
            err.contains("vulnerability_class"),
            "error should mention vulnerability_class, got: {err}"
        );
    }

    #[test]
    fn prevalidate_report_value_does_not_check_target_repo_path() {
        // Pin behavior: prevalidate_report_value only walks findings[]
        // and dedupe_groups[] looking for missing root_cause. A missing
        // target.repo_path is caught later by validate_report_struct.
        let value = serde_json::json!({
            "findings": [],
            "dedupe_groups": []
        });
        prevalidate_report_value(&value)
            .expect("prevalidate_report_value does not inspect target today");
    }

    #[test]
    fn parse_stage_json_error_includes_extracted_prefix() {
        // When the extractor returns something that is "valid JSON" (so it
        // passes the validation gate) but doesn't deserialize to the target
        // type, the error should surface what we actually parsed so future
        // debugging doesn't require turning on raw-output logging.
        // Use a small synthetic type to drive the error path.
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct Need {
            required_field: String,
        }
        let raw = r#"{"other_field": 1}"#;
        let err = parse_stage_json::<Need>(raw).unwrap_err();
        // Error message format comes from serde_json::from_value, which is
        // structurally different from from_str — make sure SOMETHING from
        // the original payload is mentioned so we can diagnose.
        assert!(
            err.contains("required_field") || err.contains("missing field"),
            "error should describe the missing field, got: {err}"
        );
    }

    // ---- Loose-shape / strict-semantic split for validate -----------------

    #[test]
    fn shape_only_parser_accepts_empty_decisions_block() {
        // No semantic gate runs here — empty decisions is a valid shape,
        // and the stage runner uses default() on shape failure anyway.
        let parsed = parse_validate_output_shape(r#"{"decisions": []}"#).unwrap();
        assert!(parsed.decisions.is_empty());
    }

    #[test]
    fn shape_only_parser_rejects_findings_field() {
        // The validator is contractually "decide on existing findings only" —
        // a model that tries to emit new findings here is misbehaving in a
        // way the next stage can't recover from cleanly.
        let raw = r#"{"findings": [{"id": "f-1"}], "decisions": []}"#;
        let err = parse_validate_output_shape(raw).unwrap_err();
        assert!(err.contains("must not include new findings"));
    }

    #[test]
    fn shape_only_parser_returns_err_on_non_json() {
        // The stage runner's job is to turn THIS into warn-and-default, not
        // to crash.  Pinning that the function itself returns Err so the
        // runner can decide.
        let err = parse_validate_output_shape("I will not be writing JSON today.").unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn semantic_gate_rejects_unknown_finding_id() {
        // Hallucination guard — the only thing the strict gate is for.
        let parsed = ValidateStageOutput {
            decisions: vec![ValidationDecision {
                finding_id: "ghost".into(),
                decision: ValidationDecisionKind::Confirmed,
                evidence: "real".into(),
                ..Default::default()
            }],
        };
        let err = validate_decisions_semantic(&parsed, &[]).unwrap_err();
        assert!(err.contains("unknown finding_id"));
    }

    #[test]
    fn semantic_gate_rejects_confirm_without_evidence() {
        let findings = vec![SecurityFinding {
            id: "f-1".into(),
            title: "thing".into(),
            severity: "HIGH".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "http".into(),
            entry_point: "POST /admin".into(),
            sink_or_decision: "auth check".into(),
            root_cause: "missing guard".into(),
            ..Default::default()
        }];
        let parsed = ValidateStageOutput {
            decisions: vec![ValidationDecision {
                finding_id: "f-1".into(),
                decision: ValidationDecisionKind::Confirmed,
                evidence: "   ".into(), // whitespace-only
                ..Default::default()
            }],
        };
        let err = validate_decisions_semantic(&parsed, &findings).unwrap_err();
        assert!(err.contains("requires evidence"));
    }

    #[test]
    fn semantic_gate_accepts_clean_confirm() {
        let findings = vec![SecurityFinding {
            id: "f-1".into(),
            title: "auth bypass on /admin".into(),
            severity: "HIGH".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "http".into(),
            entry_point: "POST /admin".into(),
            sink_or_decision: "permission check missing".into(),
            root_cause: "no guard before write".into(),
            evidence: vec!["src/handler.rs:42".into()],
            severity_rationale: "any unauthenticated user can hit it".into(),
            fix_recommendation: "add require_role(admin) to the handler".into(),
            ..Default::default()
        }];
        let parsed = ValidateStageOutput {
            decisions: vec![ValidationDecision {
                finding_id: "f-1".into(),
                decision: ValidationDecisionKind::Confirmed,
                evidence: "src/handler.rs:42 — checked, no guard".into(),
                ..Default::default()
            }],
        };
        validate_decisions_semantic(&parsed, &findings).unwrap();
    }
}
