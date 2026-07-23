//! Score durable `security_engineer` report JSON against a versioned benchmark.
//!
//! Each reports directory represents one independent sweep and contains
//! `<case-id>.json` files copied from `kb/security-harness/reports/` (or the
//! local `.dyson` fallback). Pass multiple directories to measure run-to-run
//! variance instead of grading a single lucky sample.

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;

#[derive(Parser)]
#[command(about = "Score security_engineer durable reports against a benchmark manifest")]
struct Args {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long, required = true, num_args = 1..)]
    reports_dir: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    min_root_cause_recall: f64,
    max_forbidden_match_rate: f64,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    #[serde(default)]
    expected: Vec<Expectation>,
    #[serde(default)]
    forbidden: Vec<Expectation>,
}

#[derive(Deserialize)]
struct Expectation {
    class_id: String,
    #[serde(default)]
    path_contains_any: Vec<String>,
    #[serde(default)]
    root_cause_contains_any: Vec<String>,
    #[serde(default)]
    minimum_severity: Option<String>,
}

#[derive(Deserialize)]
struct Report {
    #[serde(default)]
    findings: Vec<Finding>,
}

#[derive(Deserialize)]
struct Finding {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    vulnerability_class: String,
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    affected_paths: Vec<String>,
}

fn severity_rank(value: &str) -> Option<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "informational" | "info" => Some(0),
        "low" => Some(1),
        "medium" => Some(2),
        "high" => Some(3),
        "critical" => Some(4),
        _ => None,
    }
}

fn contains_any(haystack: &str, needles: &[String]) -> bool {
    needles.is_empty()
        || needles
            .iter()
            .any(|needle| haystack.contains(&needle.to_ascii_lowercase()))
}

fn matches_expectation(finding: &Finding, expected: &Expectation) -> bool {
    if finding.vulnerability_class != expected.class_id {
        return false;
    }
    if !expected.path_contains_any.is_empty()
        && !finding.affected_paths.iter().any(|path| {
            expected
                .path_contains_any
                .iter()
                .any(|needle| path.contains(needle))
        })
    {
        return false;
    }
    let root_cause = format!("{} {}", finding.title, finding.root_cause).to_ascii_lowercase();
    if !contains_any(&root_cause, &expected.root_cause_contains_any) {
        return false;
    }
    if let Some(minimum) = expected.minimum_severity.as_deref() {
        let Some(actual_rank) = severity_rank(&finding.severity) else {
            return false;
        };
        let Some(minimum_rank) = severity_rank(minimum) else {
            return false;
        };
        if actual_rank < minimum_rank {
            return false;
        }
    }
    true
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(&args.manifest)?)?;
    if manifest.schema_version != 1 {
        return Err(format!("unsupported benchmark schema {}", manifest.schema_version).into());
    }

    let mut expected_total = 0usize;
    let mut expected_hits = 0usize;
    let mut forbidden_total = 0usize;
    let mut forbidden_hits = 0usize;
    let mut invalid_citations = Vec::new();
    let mut missing_reports = Vec::new();

    for reports_dir in &args.reports_dir {
        for case in &manifest.cases {
            let path = reports_dir.join(format!("{}.json", case.id));
            if !path.is_file() {
                missing_reports.push(path.display().to_string());
                continue;
            }
            let report: Report = serde_json::from_slice(&std::fs::read(&path)?)?;
            let mut ids = BTreeSet::new();
            for finding in &report.findings {
                if finding.id.trim().is_empty() || !ids.insert(finding.id.as_str()) {
                    invalid_citations.push(format!(
                        "{}: empty or duplicate finding id {:?}",
                        path.display(),
                        finding.id
                    ));
                }
                if finding.affected_paths.is_empty()
                    || finding.affected_paths.iter().any(|value| {
                        !value.rsplit_once(':').is_some_and(|(_, line)| {
                            line.split('-').all(|part| part.parse::<u32>().is_ok())
                        })
                    })
                {
                    invalid_citations.push(format!(
                        "{}:{} has a missing or non-line-qualified affected path",
                        path.display(),
                        finding.id
                    ));
                }
            }
            for expected in &case.expected {
                expected_total += 1;
                if report
                    .findings
                    .iter()
                    .any(|finding| matches_expectation(finding, expected))
                {
                    expected_hits += 1;
                }
            }
            for forbidden in &case.forbidden {
                forbidden_total += 1;
                if report
                    .findings
                    .iter()
                    .any(|finding| matches_expectation(finding, forbidden))
                {
                    forbidden_hits += 1;
                }
            }
        }
    }

    let recall = if expected_total == 0 {
        1.0
    } else {
        expected_hits as f64 / expected_total as f64
    };
    let forbidden_rate = if forbidden_total == 0 {
        0.0
    } else {
        forbidden_hits as f64 / forbidden_total as f64
    };
    println!(
        "root-cause recall: {expected_hits}/{expected_total} ({:.1}%)",
        recall * 100.0
    );
    println!(
        "forbidden-match rate: {forbidden_hits}/{forbidden_total} ({:.1}%)",
        forbidden_rate * 100.0
    );
    println!("invalid citations/ids: {}", invalid_citations.len());
    println!("missing reports: {}", missing_reports.len());
    for item in missing_reports.iter().chain(invalid_citations.iter()) {
        eprintln!("- {item}");
    }

    if !missing_reports.is_empty()
        || !invalid_citations.is_empty()
        || recall < manifest.min_root_cause_recall
        || forbidden_rate > manifest.max_forbidden_match_rate
    {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expectation_requires_class_path_root_cause_and_severity() {
        let finding = Finding {
            id: "f1".into(),
            title: "Missing owner predicate".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            root_cause: "tenant id is not compared".into(),
            affected_paths: vec!["src/routes.rs:42".into()],
        };
        let expected = Expectation {
            class_id: "auth_authorization".into(),
            path_contains_any: vec!["routes.rs".into()],
            root_cause_contains_any: vec!["owner predicate".into(), "tenant id".into()],
            minimum_severity: Some("medium".into()),
        };
        assert!(matches_expectation(&finding, &expected));
    }
}
