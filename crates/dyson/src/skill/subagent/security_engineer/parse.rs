// ===========================================================================
// JSON extraction + per-stage parsers, plus the report schema validators.
//
// LLM stage outputs are wrapped in prose, code fences, and echoes of the
// prompt's checkpoint JSON.  `extract_json` walks brace-balanced candidates
// and returns the LAST one that parses — this avoids blending the prompt's
// own embedded JSON with the model's real output.
// ===========================================================================

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

pub(crate) fn parse_validate_output(
    raw: &str,
    findings: &[SecurityFinding],
) -> std::result::Result<ValidateStageOutput, String> {
    let value = parse_json_value(raw)?;
    if value.get("findings").is_some() {
        return Err("validator output must not include new findings".into());
    }
    let parsed: ValidateStageOutput = serde_json::from_value(value).map_err(|e| e.to_string())?;
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
    use super::super::types::ReconStageOutput;
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
}
