// `dependency_analysis` — discover dep manifests in a project, parse
// them, and query Google's OSV database for known vulnerabilities.
//
// Public surface: `scan()` + re-exports below.  Per-ecosystem parsers
// live in `parser/*` and are dispatched by filename in
// `detect::parser_for`.  Unknown filenames are skipped silently;
// recognised-but-unparseable files are listed in `ScanReport.unsupported`.

pub mod detect;
pub mod osv;
pub mod parser;
pub mod types;

pub use osv::{OsvClient, OsvError};
pub use types::{Dependency, Ecosystem, ParseError, Parsed, ScanReport, Severity, Vulnerability};

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::error::{DysonError, Result};

pub struct ScanOptions {
    pub recursive: bool,
    /// Upper bound on how many manifest files to parse.
    pub max_files: usize,
    /// Drop findings below this threshold before returning.  OSV is
    /// still queried for everything; this only trims the output.
    pub severity_min: Severity,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            max_files: 200,
            severity_min: Severity::Unknown,
        }
    }
}

/// Walk `root` (file or directory), parse every recognised manifest,
/// query OSV, and return a full [`ScanReport`].  Per-file parse
/// problems are recorded in `warnings` / `unsupported`; the outer
/// `Err` is reserved for I/O on `root` itself.
pub async fn scan(root: &Path, opts: &ScanOptions, client: &OsvClient) -> Result<ScanReport> {
    let manifests = discover(root, opts)?;
    let mut report = ScanReport::default();
    let mut all_deps: Vec<Dependency> = Vec::new();

    for path in manifests {
        let Some(parser) = detect::parser_for(&path) else {
            continue; // unrecognised; skip silently
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                report
                    .warnings
                    .push(format!("{}: read failed: {e}", path.display()));
                continue;
            }
        };
        match parser.parse(&path, &bytes) {
            Ok(parsed) => {
                report.scanned_files.push(path);
                report.warnings.extend(parsed.warnings);
                all_deps.extend(parsed.deps);
            }
            Err(e) => {
                // Malformed == unsupported for the agent: we recognised
                // the file but couldn't extract trustworthy data.
                report.warnings.push(format!("{e}"));
                report.unsupported.push(path);
            }
        }
    }

    report.deps_total = all_deps.len();
    let queryable: Vec<&Dependency> = all_deps
        .iter()
        .filter(|d| d.purl.is_some() || d.version.is_some())
        .collect();
    report.deps_queried = queryable.len();
    if queryable.is_empty() {
        return Ok(report);
    }

    let id_matrix = match client.querybatch(&queryable).await {
        Ok(m) => m,
        Err(e) => {
            report.warnings.push(format!("OSV querybatch failed: {e}"));
            return Ok(report);
        }
    };

    // fetch_details dedupes internally; no need to pre-sort.
    let flat_ids: Vec<String> = id_matrix.iter().flatten().cloned().collect();
    let (details, detail_warnings) = client.fetch_details(&flat_ids).await;
    report.warnings.extend(detail_warnings);

    let index: std::collections::HashMap<&str, &Vulnerability> =
        details.iter().map(|v| (v.id.as_str(), v)).collect();

    for (dep, ids) in queryable.iter().zip(id_matrix.iter()) {
        if ids.is_empty() {
            continue;
        }
        let mut vulns: Vec<Vulnerability> = ids
            .iter()
            .filter_map(|id| index.get(id.as_str()).map(|v| (*v).clone()))
            .filter(|v| v.severity >= opts.severity_min)
            .collect();
        vulns.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
        if !vulns.is_empty() {
            report.findings.push(((*dep).clone(), vulns));
        }
    }

    Ok(report)
}

fn discover(root: &Path, opts: &ScanOptions) -> Result<Vec<PathBuf>> {
    let meta = std::fs::metadata(root).map_err(|e| {
        DysonError::tool("dependency_scan", format!("stat {}: {e}", root.display()))
    })?;
    if meta.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }

    let mut builder = WalkBuilder::new(root);
    builder.hidden(false); // we need .github/workflows
    if !opts.recursive {
        builder.max_depth(Some(1));
    }

    let mut out = Vec::new();
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if detect::parser_for(entry.path()).is_some() {
            out.push(entry.path().to_path_buf());
            if out.len() >= opts.max_files {
                break;
            }
        }
    }
    Ok(out)
}
