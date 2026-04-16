// ===========================================================================
// Post-validator for the security_engineer orchestrator's final report.
//
// The prompt does most of the work (Pre-Flag Checklist, Pre-Submit Self-
// Check).  This module adds a cheap pure-text scan that catches the
// handful of failure modes we can detect without re-reading files.  When
// it flags anything, the orchestrator re-spawns the child up to
// `max_validator_retries` times with the issues as a fix-this prompt; any
// residual issues after that are appended to the report as an appendix.
// ===========================================================================

use regex::Regex;
use std::sync::LazyLock;

/// Entry point.  Given the child agent's final text, return one human-
/// readable string per validation failure.  Empty vec = nothing to report.
pub fn validate_report(text: &str) -> Vec<String> {
    let mut issues = Vec::new();
    check_exec_summary_math(text, &mut issues);
    check_duplicate_file_line_headers(text, &mut issues);
    check_markdown_link_parity(text, &mut issues);
    check_severity_hedging(text, &mut issues);
    check_attack_tree_present(text, &mut issues);
    check_exploit_field_for_dangerous_sinks(text, &mut issues);
    check_dep_linked_findings(text, &mut issues);
    issues
}

// ---------------------------------------------------------------------------
// Shared slicing helpers (borrow from `text`, no allocations)
// ---------------------------------------------------------------------------

/// Yield `(header, body)` pairs for each `## HEADER` block in the report.
fn sections(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut current: Option<(&str, usize)> = None; // (header, body_start_idx)
    for line in text.split_inclusive('\n') {
        let line_start = cursor;
        cursor += line.len();
        if let Some(rest) = line.trim_end_matches('\n').strip_prefix("## ") {
            if let Some((h, start)) = current.take() {
                out.push((h, &text[start..line_start]));
            }
            current = Some((rest.trim(), cursor));
        }
    }
    if let Some((h, start)) = current {
        out.push((h, &text[start..]));
    }
    out
}

/// Split a section body into top-level `- ` bullet entries (each entry is
/// a contiguous slice from one bullet to just before the next).  Covers
/// both in-code findings (`- [file:line] …`) and dep entries
/// (`- npm name@version — …`).
fn split_entries(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut cursor = 0;
    for line in body.split_inclusive('\n') {
        let line_start = cursor;
        cursor += line.len();
        let is_top_bullet = line.starts_with("- ");
        if is_top_bullet {
            if let Some(s) = start {
                out.push(&body[s..line_start]);
            }
            start = Some(line_start);
        }
    }
    if let Some(s) = start {
        out.push(&body[s..]);
    }
    out
}

/// True when an entry's first line is a `- [file:line]` code-finding header.
fn is_code_finding(entry: &str) -> bool {
    entry.starts_with("- [")
}

/// The first line of an entry, trimmed — used in issue messages.
fn head(entry: &str) -> &str {
    entry.lines().next().unwrap_or("<unnamed>").trim()
}

/// Count top-level code-finding bullets (`- [file:line]`) in a section body.
fn count_code_findings(body: &str) -> usize {
    split_entries(body).into_iter().filter(|e| is_code_finding(e)).count()
}

/// Run `f` for every code finding under a matching severity section.
fn for_each_code_finding<'a>(
    text: &'a str,
    severities: &[&str],
    mut f: impl FnMut(&'a str, &'a str),
) {
    for (header, body) in sections(text) {
        if !severities.iter().any(|s| header.eq_ignore_ascii_case(s)) {
            continue;
        }
        for entry in split_entries(body) {
            if is_code_finding(entry) {
                f(header, entry);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Check 1: Executive-summary math guard
// ---------------------------------------------------------------------------

static EXEC_SUMMARY_COUNT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(\d+)\s+(critical(?:\s+and\s+high[-\w]*)?|high[-\w]*|medium[-\w]*|low[-\w]*|findings?)").unwrap()
});

fn check_exec_summary_math(text: &str, issues: &mut Vec<String>) {
    let window_end = ["## CRITICAL", "## HIGH", "## MEDIUM"]
        .iter()
        .find_map(|h| text.find(h))
        .unwrap_or_else(|| text.len().min(1500));
    let window = &text[..window_end];

    let secs = sections(text);
    let count = |name: &str| -> usize {
        secs.iter()
            .find(|(h, _)| h.eq_ignore_ascii_case(name))
            .map(|(_, b)| count_code_findings(b))
            .unwrap_or(0)
    };
    let crit = count("CRITICAL");
    let high = count("HIGH");
    let medium = count("MEDIUM");
    let low = count("LOW / INFORMATIONAL");
    let total = crit + high + medium + low;

    for m in EXEC_SUMMARY_COUNT_RE.captures_iter(window) {
        let n: usize = m[1].parse().unwrap_or(0);
        let label = m[2].to_lowercase();
        let expected = match label.as_str() {
            l if l.starts_with("critical") && l.contains("high") => crit + high,
            l if l.starts_with("critical") => crit,
            l if l.starts_with("high") => high,
            l if l.starts_with("medium") => medium,
            l if l.starts_with("low") => low,
            l if l.starts_with("finding") => total,
            _ => continue,
        };
        if n != expected {
            issues.push(format!(
                "executive summary says \"{n} {label}\" but body contains {expected} matching finding(s)"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Check 2: Duplicate [file:line] headers
// ---------------------------------------------------------------------------

static FILE_LINE_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*-\s+\[([^\]]+)\]").unwrap());

fn check_duplicate_file_line_headers(text: &str, issues: &mut Vec<String>) {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for m in FILE_LINE_HEADER_RE.captures_iter(text) {
        *seen.entry(m[1].trim().to_string()).or_insert(0) += 1;
    }
    for (key, n) in seen {
        if n > 1 {
            issues.push(format!(
                "duplicate finding header `[{key}]` appears {n} times — merge into one finding at the higher severity"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Check 3: Markdown link parity
// ---------------------------------------------------------------------------

static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());

/// True for "path/to/file.ts:42" — path with `/` or `.`, then `:`, then digits.
fn path_line_prefix(s: &str) -> Option<&str> {
    let (path, rest) = s.trim().rsplit_once(':')?;
    let is_line_num = !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    if is_line_num && (path.contains('/') || path.contains('.')) {
        Some(path)
    } else {
        None
    }
}

fn check_markdown_link_parity(text: &str, issues: &mut Vec<String>) {
    for m in MD_LINK_RE.captures_iter(text) {
        let (display, href) = (m[1].trim(), m[2].trim());
        if let (Some(dp), Some(hp)) = (path_line_prefix(display), path_line_prefix(href))
            && dp != hp
        {
            issues.push(format!(
                "markdown link paths disagree: `[{display}]({href})` — display path `{dp}` does not match href path `{hp}`"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Check 4: Severity hedging at CRITICAL
// ---------------------------------------------------------------------------

static HEDGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(may allow|may\s|might\b|could\b|potentially\b|potential\b|limited\b|in theory\b|if the attacker)").unwrap()
});

fn check_severity_hedging(text: &str, issues: &mut Vec<String>) {
    for_each_code_finding(text, &["CRITICAL"], |_sev, finding| {
        let Some(impact) = finding.lines().find(|l| l.trim_start().starts_with("Impact:")) else {
            return;
        };
        if let Some(hit) = HEDGE_RE.find(impact) {
            issues.push(format!(
                "CRITICAL finding `{}` has hedged Impact (\"{}\") — downgrade severity or rewrite Impact as a concrete attacker outcome",
                head(finding), hit.as_str()
            ));
        }
    });
}

// ---------------------------------------------------------------------------
// Check 5: Attack Tree present for CRITICAL/HIGH findings
// ---------------------------------------------------------------------------

fn check_attack_tree_present(text: &str, issues: &mut Vec<String>) {
    for_each_code_finding(text, &["CRITICAL", "HIGH"], |sev, finding| {
        if !finding.contains("Attack Tree:") {
            issues.push(format!(
                "{sev} finding `{}` is missing its `Attack Tree:` block",
                head(finding)
            ));
        }
    });
}

// ---------------------------------------------------------------------------
// Check 6: Exploit field present for dangerous-sink findings
// ---------------------------------------------------------------------------

static DANGEROUS_SINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beval\b|\bexec\b|\bFunction\(|\$where|vm\.runIn|JSON\.parse|deserialize|pickle\.loads?|yaml\.load\b").unwrap()
});

fn check_exploit_field_for_dangerous_sinks(text: &str, issues: &mut Vec<String>) {
    for_each_code_finding(text, &["CRITICAL", "HIGH"], |sev, finding| {
        if DANGEROUS_SINK_RE.is_match(finding) && !finding.contains("Exploit:") {
            issues.push(format!(
                "{sev} finding `{}` references a dangerous sink (eval/exec/Function/$where/deserialize/etc.) but has no `Exploit:` line",
                head(finding)
            ));
        }
    });
}

// ---------------------------------------------------------------------------
// Check 7: Dependency cross-link field presence
// ---------------------------------------------------------------------------

fn check_dep_linked_findings(text: &str, issues: &mut Vec<String>) {
    // Gate: only run when the report actually has a dep-review block.
    let secs = sections(text);
    let has_dep = secs.iter().any(|(h, _)| h.to_lowercase().contains("dependenc"));
    if !has_dep && !text.contains("linked-findings:") {
        return;
    }

    for (header, body) in &secs {
        let is_dep_sev = header.eq_ignore_ascii_case("Critical")
            || header.eq_ignore_ascii_case("High")
            || header.to_lowercase().contains("dependenc");
        if !is_dep_sev {
            continue;
        }
        for entry in split_entries(body) {
            // Dep entries are top-level bullets that are NOT code findings
            // and carry an `@version` marker.
            if is_code_finding(entry) {
                continue;
            }
            let first = head(entry);
            if !first.contains('@') {
                continue;
            }
            if !entry.contains("linked-findings:") {
                issues.push(format!(
                    "dependency entry `{first}` under `## {header}` is missing the `linked-findings:` field (use `unreferenced` if no in-code finding exercises it)"
                ));
            }
        }
    }
}

// ===========================================================================
// Unit tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_flag(text: &str, needle: &str) {
        let issues = validate_report(text);
        assert!(
            issues.iter().any(|i| i.contains(needle)),
            "expected issue containing `{needle}`, got {issues:?}"
        );
    }

    fn expect_no_flag(text: &str, needle: &str) {
        let issues = validate_report(text);
        assert!(
            !issues.iter().any(|i| i.contains(needle)),
            "unexpected issue containing `{needle}`: {issues:?}"
        );
    }

    const PASSING: &str = r#"Executive summary: 1 critical and high-severity issue was found.

## CRITICAL
- [foo.ts:10] SQL injection
  Evidence: `db.query("select " + x)`
  Attack Tree:
    entry.ts:5 — req.query.x
      └─ foo.ts:10 — string-interpolated SQL
  Impact: Attacker reads the users table.
  Exploit: `curl /foo?x=1;drop table users`
  Remediation: use parameterized queries.
"#;

    #[test]
    fn passing_report_has_no_issues() {
        assert!(validate_report(PASSING).is_empty());
    }

    #[test]
    fn exec_summary_math_mismatch_flagged() {
        expect_flag(
            r#"Executive summary: 5 critical and high-severity findings.

## CRITICAL
- [foo.ts:10] one
  Attack Tree: entry — sink
  Impact: complete compromise.
"#,
            "executive summary says",
        );
    }

    #[test]
    fn duplicate_file_line_header_flagged() {
        expect_flag(
            r#"## CRITICAL
- [foo.ts:10] one
  Attack Tree: x — y
  Impact: compromise.
- [foo.ts:10] duplicate
  Attack Tree: x — y
  Impact: compromise.
"#,
            "duplicate finding header",
        );
    }

    #[test]
    fn markdown_link_path_mismatch_flagged() {
        expect_flag(
            "See [foo.ts:10](bar.ts:10) for details.",
            "markdown link paths disagree",
        );
    }

    #[test]
    fn markdown_link_parity_passes_when_paths_match() {
        assert!(validate_report("See [foo.ts:10](foo.ts:10) for details.").is_empty());
    }

    #[test]
    fn hedged_critical_impact_flagged() {
        expect_flag(
            r#"## CRITICAL
- [foo.ts:10] SQL injection
  Attack Tree: entry — sink
  Impact: Attacker may allow limited data exfiltration if the attacker has a valid cookie.
  Exploit: `curl ...`
"#,
            "hedged Impact",
        );
    }

    #[test]
    fn missing_attack_tree_flagged() {
        expect_flag(
            r#"## CRITICAL
- [foo.ts:10] SQL injection
  Impact: Attacker reads user table.
  Exploit: `curl ...`
"#,
            "missing its `Attack Tree:` block",
        );
    }

    #[test]
    fn missing_exploit_on_eval_finding_flagged() {
        expect_flag(
            r#"## CRITICAL
- [foo.ts:10] eval of user input
  Evidence: `eval(req.query.x)`
  Attack Tree: entry — sink
  Impact: RCE from HTTP.
"#,
            "dangerous sink",
        );
    }

    #[test]
    fn missing_linked_findings_in_dep_section_flagged() {
        expect_flag(
            r#"## Dependency Review
see below

## Critical
- npm jsonwebtoken@0.4.0 — GHSA-xxx — signature bypass  [fixed in: 9.0.0]
  context: used in routes/login.ts
"#,
            "linked-findings",
        );
    }

    #[test]
    fn dep_entry_with_unreferenced_passes() {
        expect_no_flag(
            r#"## Dependency Review

## Critical
- npm jsonwebtoken@0.4.0 — GHSA-xxx — signature bypass  [fixed in: 9.0.0]
  context: present in lockfile
  linked-findings: unreferenced
"#,
            "linked-findings",
        );
    }

    #[test]
    fn code_finding_headers_not_mistaken_for_dep_entries() {
        expect_no_flag(PASSING, "linked-findings");
    }
}
