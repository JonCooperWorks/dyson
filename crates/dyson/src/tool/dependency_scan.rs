// dependency_scan — agent-facing wrapper around
// `crate::dependency_analysis::scan`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::dependency_analysis::{
    self, Dependency, OsvClient, ScanOptions, ScanReport, Severity,
};
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
                "format": {
                    "type": "string",
                    "enum": ["text","json","cyclonedx"],
                    "default": "text",
                    "description": "text: human report. json: full ScanReport. cyclonedx: CycloneDX 1.5 JSON SBOM with components + vulnerabilities."
                },
                "severity_min": { "type": "string", "enum": ["low","medium","high","critical"] }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: Input = serde_json::from_value(input.clone())
            .map_err(|e| DysonError::tool("dependency_scan", format!("invalid input: {e}")))?;
        let root = match ctx.resolve_path(&parsed.path) { Ok(p) => p, Err(e) => return Ok(e) };
        let opts = ScanOptions {
            recursive: parsed.recursive,
            severity_min: parse_severity(parsed.severity_min.as_deref()),
            ..ScanOptions::default()
        };
        let client = OsvClient::new();
        let report = dependency_analysis::scan(&root, &opts, &client).await?;

        let out = match parsed.format.as_deref() {
            Some("json") => serde_json::to_string_pretty(&report)
                .map_err(|e| DysonError::tool("dependency_scan", format!("json encode: {e}")))?,
            Some("cyclonedx") => serde_json::to_string_pretty(&render_cyclonedx(&report))
                .map_err(|e| DysonError::tool("dependency_scan", format!("cyclonedx encode: {e}")))?,
            _ => render_text(&report),
        };
        Ok(ToolOutput::success(out))
    }
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

/// Stable identifier for a dep in CycloneDX output.  PURL when the
/// parser gave us one; otherwise synthesize from ecosystem/name@version.
fn bom_ref(dep: &Dependency) -> String {
    if let Some(p) = dep.purl.as_deref() {
        return p.to_string();
    }
    let ty = dep.ecosystem.to_purl_type().unwrap_or("generic");
    match dep.version.as_deref() {
        Some(v) => format!("pkg:{ty}/{}@{v}", dep.name),
        None => format!("pkg:{ty}/{}", dep.name),
    }
}

fn render_cyclonedx(report: &ScanReport) -> serde_json::Value {
    let components: Vec<serde_json::Value> = report
        .deps
        .iter()
        .map(|d| {
            let mut c = json!({
                "type": "library",
                "bom-ref": bom_ref(d),
                "name": d.name,
            });
            if let Some(v) = &d.version {
                c["version"] = json!(v);
            }
            if let Some(p) = &d.purl {
                c["purl"] = json!(p);
            }
            c
        })
        .collect();

    let vulnerabilities: Vec<serde_json::Value> = report
        .findings
        .iter()
        .flat_map(|(dep, vulns)| {
            let dep_ref = bom_ref(dep);
            vulns.iter().map(move |v| {
                let mut entry = json!({
                    "id": v.id,
                    "source": { "name": "OSV", "url": format!("https://osv.dev/vulnerability/{}", v.id) },
                    "ratings": [{ "severity": v.severity.as_str() }],
                    "description": v.summary,
                    "affects": [{ "ref": dep_ref }],
                });
                if !v.fixed_versions.is_empty() {
                    entry["recommendation"] =
                        json!(format!("Upgrade to {}", v.fixed_versions.join(", ")));
                }
                if !v.references.is_empty() {
                    entry["references"] = json!(
                        v.references.iter().map(|r| json!({ "url": r })).collect::<Vec<_>>()
                    );
                }
                entry
            })
        })
        .collect();

    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "components": components,
        "vulnerabilities": vulnerabilities,
    })
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

    fn sample_report() -> ScanReport {
        let dep = Dependency {
            name: "foo".into(),
            version: Some("1.0.0".into()),
            ecosystem: Ecosystem::CratesIo,
            purl: None,
            source_file: PathBuf::from("Cargo.lock"),
            direct: true,
        };
        let clean = Dependency {
            name: "bar".into(),
            version: Some("2.1.0".into()),
            ecosystem: Ecosystem::Npm,
            purl: Some("pkg:npm/bar@2.1.0".into()),
            source_file: PathBuf::from("package-lock.json"),
            direct: true,
        };
        let vuln = Vulnerability {
            id: "GHSA-xxxx".into(),
            aliases: vec![],
            summary: "boom".into(),
            severity: Severity::High,
            affected_ranges: vec![],
            references: vec!["https://example.invalid/advisory".into()],
            fixed_versions: vec!["1.0.1".into()],
        };
        let mut report = ScanReport::default();
        report.deps.push(dep.clone());
        report.deps.push(clean);
        report.findings.push((dep, vec![vuln]));
        report.deps_total = 2;
        report.deps_queried = 2;
        report
    }

    #[test]
    fn cyclonedx_has_required_envelope() {
        let v = render_cyclonedx(&sample_report());
        assert_eq!(v["bomFormat"], "CycloneDX");
        assert_eq!(v["specVersion"], "1.5");
        assert_eq!(v["version"], 1);
        assert!(v["components"].is_array());
        assert!(v["vulnerabilities"].is_array());
    }

    #[test]
    fn cyclonedx_lists_all_components_not_just_vulnerable() {
        let v = render_cyclonedx(&sample_report());
        let names: Vec<&str> = v["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn cyclonedx_synthesizes_purl_when_missing() {
        let v = render_cyclonedx(&sample_report());
        let foo = &v["components"][0];
        assert_eq!(foo["bom-ref"], "pkg:cargo/foo@1.0.0");
        assert!(foo.get("purl").is_none(), "no purl to pass through");
        let bar = &v["components"][1];
        assert_eq!(bar["bom-ref"], "pkg:npm/bar@2.1.0");
        assert_eq!(bar["purl"], "pkg:npm/bar@2.1.0");
    }

    #[test]
    fn cyclonedx_affects_ref_matches_component_bom_ref() {
        let v = render_cyclonedx(&sample_report());
        let affects_ref = v["vulnerabilities"][0]["affects"][0]["ref"]
            .as_str()
            .unwrap()
            .to_string();
        let component_ref = v["components"][0]["bom-ref"].as_str().unwrap().to_string();
        assert_eq!(affects_ref, component_ref);
    }

    #[test]
    fn cyclonedx_vuln_carries_severity_and_fix() {
        let v = render_cyclonedx(&sample_report());
        let vuln = &v["vulnerabilities"][0];
        assert_eq!(vuln["id"], "GHSA-xxxx");
        assert_eq!(vuln["ratings"][0]["severity"], "high");
        assert_eq!(vuln["source"]["name"], "OSV");
        assert!(
            vuln["recommendation"]
                .as_str()
                .unwrap()
                .contains("1.0.1")
        );
        assert_eq!(
            vuln["references"][0]["url"],
            "https://example.invalid/advisory"
        );
    }
}
