// dependency_scan — agent-facing wrapper around
// `crate::dependency_analysis::scan`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::dependency_analysis::{self, OsvClient, ScanOptions, ScanReport, Severity};
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

#[derive(Deserialize)]
struct Input {
    path: String,
    #[serde(default = "default_recursive")]
    recursive: bool,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    severity_min: Option<String>,
}

fn default_recursive() -> bool {
    true
}

pub struct DependencyScanTool;

#[async_trait]
impl Tool for DependencyScanTool {
    fn name(&self) -> &str {
        "dependency_scan"
    }

    fn description(&self) -> &str {
        "Scan dependency manifests and lockfiles for known vulnerabilities via Google's \
         OSV database.  Accepts a file or directory path; walks directories recursively \
         (respecting .gitignore).  Supports every ecosystem OSV tracks — Cargo, npm, \
         PyPI, Go, Maven, NuGet, RubyGems, Packagist, Pub, Hex, CRAN, SwiftURL, \
         GitHub Actions, Hackage, ConanCenter, and any ecosystem via CycloneDX/SPDX \
         SBOMs.  Filenames the scanner doesn't recognise are skipped silently; \
         manifests it recognises but can't parse without guessing are listed as \
         'unsupported' rather than producing unreliable results."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Manifest file OR project directory" },
                "recursive": { "type": "boolean", "default": true },
                "format": { "type": "string", "enum": ["text","json"], "default": "text" },
                "severity_min": { "type": "string", "enum": ["low","medium","high","critical"] }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: Input = serde_json::from_value(input.clone())
            .map_err(|e| DysonError::tool("dependency_scan", format!("invalid input: {e}")))?;
        let root = resolve_path(&ctx.working_dir, &parsed.path);
        let opts = ScanOptions {
            recursive: parsed.recursive,
            severity_min: parse_severity(parsed.severity_min.as_deref()),
            ..ScanOptions::default()
        };
        let client = OsvClient::new();
        let report = dependency_analysis::scan(&root, &opts, &client).await?;

        let out = if parsed.format.as_deref() == Some("json") {
            serde_json::to_string_pretty(&report)
                .map_err(|e| DysonError::tool("dependency_scan", format!("json encode: {e}")))?
        } else {
            render_text(&report)
        };
        Ok(ToolOutput::success(out))
    }
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

fn parse_severity(s: Option<&str>) -> Severity {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("critical") => Severity::Critical,
        Some("high") => Severity::High,
        Some("medium") => Severity::Medium,
        Some("low") => Severity::Low,
        _ => Severity::Unknown,
    }
}

fn render_text(report: &ScanReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Dependency scan\n\nfiles scanned: {}  |  deps: {} total, {} queried  |  findings: {}",
        report.scanned_files.len(),
        report.deps_total,
        report.deps_queried,
        report.findings.len()
    );

    if report.scanned_files.is_empty() && report.unsupported.is_empty() {
        let _ = writeln!(out, "\nNO_MANIFESTS_FOUND");
        return out;
    }

    let mut by_sev: std::collections::BTreeMap<Severity, Vec<String>> =
        std::collections::BTreeMap::new();
    for (dep, vulns) in &report.findings {
        for v in vulns {
            let ver = dep
                .version
                .as_deref()
                .map(|v| format!("@{v}"))
                .unwrap_or_default();
            let fix = if v.fixed_versions.is_empty() {
                String::new()
            } else {
                format!("  [fixed in: {}]", v.fixed_versions.join(", "))
            };
            by_sev.entry(v.severity).or_default().push(format!(
                "- {} {}{} — {} ({}){}",
                dep.ecosystem.osv_id(),
                dep.name,
                ver,
                v.id,
                v.summary.chars().take(160).collect::<String>(),
                fix,
            ));
        }
    }

    for sev in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Unknown,
    ] {
        if let Some(lines) = by_sev.get(&sev) {
            let _ = writeln!(
                out,
                "\n## {} ({})",
                sev.as_str().to_uppercase(),
                lines.len()
            );
            for l in lines {
                let _ = writeln!(out, "{l}");
            }
        }
    }

    if !report.unsupported.is_empty() {
        let _ = writeln!(out, "\n## Unsupported");
        for p in &report.unsupported {
            let _ = writeln!(out, "- {}", p.display());
        }
    }
    if !report.warnings.is_empty() {
        let _ = writeln!(out, "\n## Warnings");
        for w in &report.warnings {
            let _ = writeln!(out, "- {w}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency_analysis::types::{Dependency, Ecosystem, ScanReport, Vulnerability};
    use std::path::PathBuf;

    #[test]
    fn text_output_empty_is_no_manifests_found() {
        let txt = render_text(&ScanReport::default());
        assert!(txt.contains("NO_MANIFESTS_FOUND"));
    }

    #[test]
    fn text_output_groups_by_severity() {
        let dep = Dependency {
            name: "foo".into(),
            version: Some("1.0.0".into()),
            ecosystem: Ecosystem::CratesIo,
            purl: None,
            source_file: PathBuf::from("Cargo.lock"),
            direct: true,
        };
        let vuln = Vulnerability {
            id: "GHSA-xxxx".into(),
            aliases: vec![],
            summary: "boom".into(),
            severity: Severity::High,
            affected_ranges: vec![],
            references: vec![],
            fixed_versions: vec!["1.0.1".into()],
        };
        let mut report = ScanReport::default();
        report.scanned_files.push(PathBuf::from("Cargo.lock"));
        report.deps_total = 1;
        report.deps_queried = 1;
        report.findings.push((dep, vec![vuln]));
        let txt = render_text(&report);
        assert!(txt.contains("## HIGH"));
        assert!(txt.contains("GHSA-xxxx"));
        assert!(txt.contains("fixed in: 1.0.1"));
    }
}
