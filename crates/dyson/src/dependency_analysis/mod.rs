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
    let discovered = discover(root, opts)?;
    // Drop constraint-only manifests when an authoritative lockfile of
    // the same ecosystem is in the scan.  A Cargo workspace's root
    // `Cargo.lock` pins every member's deps; parsing the members'
    // `Cargo.toml` in addition is not only redundant, it's actively
    // wrong — version strings like `"1"` or `"0.3"` are ranges, not
    // exact versions, and OSV returns "every advisory that ever
    // matched tokio 1.x" when queried with `"1"`.  The UI then shows
    // those as unknown-severity false positives alongside the real
    // lockfile scan.  Same pattern for npm/yarn/pnpm.
    let manifests = filter_redundant_manifests(discovered);
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

    // Ecosystem-level lockfile hints — one warning per ecosystem, not
    // per manifest.  A Cargo workspace keeps its lockfile at the root
    // and every member's `Cargo.toml` resolves through it, so warning
    // per-Cargo.toml (the old behaviour) fired spuriously with "no
    // Cargo.lock present in any crate" even when the workspace had a
    // root lockfile.  Same structural pattern for npm/yarn/pnpm.
    append_lockfile_warnings(&mut report);

    report.deps_total = all_deps.len();
    // Belt-and-braces: refuse to query OSV with non-exact version
    // strings.  A `version: "1"` from a stray constraint-only
    // manifest would otherwise trigger "match everything" on the
    // OSV side and produce unknown-severity false positives.  Real
    // lockfile versions (`1.52.1`, `0.3.32`, …) pass this gate.
    let queryable: Vec<&Dependency> = all_deps
        .iter()
        .filter(|d| d.purl.is_some() || d.version.as_deref().is_some_and(is_exact_version))
        .collect();
    report.deps_queried = queryable.len();
    if queryable.is_empty() {
        drop(queryable);
        report.deps = all_deps;
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

    drop(queryable);
    report.deps = all_deps;
    Ok(report)
}

/// Emit ONE warning per ecosystem when a manifest was scanned but no
/// matching lockfile was found anywhere in the scan.  Replaces the old
/// per-file warnings in the individual parsers, which couldn't see
/// sibling files and so mis-fired on workspace members whose lockfile
/// lives at the project root.
///
/// Policy: if `manifest_any` && `!lock_any` → one warning.  Silence
/// when either no manifest was seen (nothing to warn about) or a
/// lockfile was seen (versions are pinned somewhere in the tree, even
/// if not alongside every member manifest).
fn append_lockfile_warnings(report: &mut ScanReport) {
    let has_name = |names: &[&str]| -> bool {
        report.scanned_files.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.iter().any(|want| n.eq_ignore_ascii_case(want)))
        })
    };
    // Cargo: `Cargo.toml` is a constraint manifest; `Cargo.lock` is
    // the resolved lockfile.  One workspace lock covers every member,
    // so we only warn when NO `Cargo.lock` was seen in the scan.
    if has_name(&["Cargo.toml"]) && !has_name(&["Cargo.lock"]) {
        report.warnings.push(
            "no Cargo.lock found in scan — versions may drift on the next `cargo update`. \
             In a Cargo workspace the lockfile lives at the workspace root; if you expected \
             one, re-run the scan against the workspace root."
                .to_string(),
        );
    }
    // npm / yarn / pnpm: any of the three lockfile flavours satisfies
    // the "pinned somewhere" check.
    if has_name(&["package.json"])
        && !has_name(&["package-lock.json", "yarn.lock", "pnpm-lock.yaml"])
    {
        report.warnings.push(
            "no npm/yarn/pnpm lockfile found in scan — package.json carries ranges, not \
             pinned versions.  Commit a lockfile for reproducible builds."
                .to_string(),
        );
    }
}

/// Drop constraint-only manifests when an authoritative lockfile of
/// the same ecosystem is in the discovered set.  Rationale:
///
/// - Cargo's lockfile is workspace-wide.  The root `Cargo.lock`
///   already has every member's resolved version; scanning the
///   member `Cargo.toml`s adds redundant constraint strings
///   (`"1"`, `"0.3"`) that OSV interprets as wildcards.
/// - npm / yarn / pnpm follow the same pattern: one lockfile covers
///   the workspace; `package.json` holds semver ranges, not pins.
///
/// We can't suppress the entries entirely — the agent-visible tool
/// output lists `scanned_files`, and dropping the constraint manifest
/// from that list would be a silent change.  Instead we skip the
/// parse (no deps contributed) and leave a trace via the existing
/// `append_lockfile_warnings` summary.
fn filter_redundant_manifests(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let name_of = |p: &Path| -> Option<String> {
        p.file_name().and_then(|n| n.to_str()).map(|s| s.to_ascii_lowercase())
    };
    let has = |target: &str| -> bool {
        paths.iter().any(|p| name_of(p).is_some_and(|n| n == target))
    };
    let cargo_lock = has("cargo.lock");
    let npm_lock =
        has("package-lock.json") || has("yarn.lock") || has("pnpm-lock.yaml");
    paths
        .into_iter()
        .filter(|p| {
            let Some(name) = name_of(p) else { return true };
            // Cargo.toml without authoritative lockfile stays; with
            // one, it's redundant and gets dropped.
            if name == "cargo.toml" && cargo_lock {
                return false;
            }
            if name == "package.json" && npm_lock {
                return false;
            }
            true
        })
        .collect()
}

/// True iff `v` is an exact semver-looking version (`1.52.1`,
/// `0.3.32+git.abc`, `2.0.0-alpha.1`).  Rejects range / prefix
/// specifiers that are legal in Cargo.toml / package.json but not in
/// OSV's `version` parameter: `"1"`, `"^1.2"`, `">=0.3"`, `"~1"`,
/// `"1.*"`, `"1 || 2"`, the empty string, etc.  Only used as a
/// defensive gate before sending to OSV — the `filter_redundant_manifests`
/// step should already have removed the usual source of non-exact
/// versions (constraint manifests in lockfile-covered trees).
fn is_exact_version(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() {
        return false;
    }
    // Any range operator or wildcard → not exact.
    if v.contains(|c: char| matches!(c, '^' | '~' | '*' | '<' | '>' | '=' | '|' | ' ' | ','))
    {
        return false;
    }
    // Must have at least `MAJOR.MINOR.PATCH`.  Semver's pre-release
    // (`-alpha`) and build-metadata (`+git.abc`) suffixes are fine —
    // we only require three numeric leading components separated
    // by `.`.
    let core_end = v
        .find(|c: char| c == '-' || c == '+')
        .unwrap_or(v.len());
    let core = &v[..core_end];
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    parts.iter().take(3).all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(files: &[&str]) -> ScanReport {
        let mut r = ScanReport::default();
        for f in files {
            r.scanned_files.push(PathBuf::from(f));
        }
        r
    }

    #[test]
    fn no_lockfile_warning_skipped_when_workspace_root_has_one() {
        // Regression: the old cargo parser emitted a per-Cargo.toml
        // warning that fired for every workspace member, even though
        // the workspace root's `Cargo.lock` already pinned versions.
        let mut r = report_with(&[
            "Cargo.lock",
            "crates/a/Cargo.toml",
            "crates/b/Cargo.toml",
        ]);
        append_lockfile_warnings(&mut r);
        assert!(
            r.warnings.is_empty(),
            "lockfile warning must not fire when Cargo.lock is present: {:?}",
            r.warnings,
        );
    }

    #[test]
    fn no_lockfile_warning_when_only_cargo_toml_and_no_lock() {
        // Genuine case: no lockfile anywhere in the scan.
        let mut r = report_with(&["Cargo.toml"]);
        append_lockfile_warnings(&mut r);
        assert_eq!(r.warnings.len(), 1, "one workspace-level warning expected");
        assert!(
            r.warnings[0].contains("no Cargo.lock found"),
            "message must name the file: {:?}",
            r.warnings,
        );
    }

    #[test]
    fn no_warnings_when_neither_manifest_nor_lock_present() {
        // E.g. scan of a non-Rust repo — nothing to warn about.
        let mut r = report_with(&["package.json", "package-lock.json"]);
        append_lockfile_warnings(&mut r);
        assert!(r.warnings.is_empty(), "no-op when manifest absent: {:?}", r.warnings);
    }

    #[test]
    fn redundant_cargo_toml_is_dropped_when_workspace_lockfile_present() {
        // Regression: the false-positive dump where constraint
        // strings like "tokio 1" / "futures-util 0.3" got flagged
        // with unknown severity because OSV matched everything when
        // handed a non-exact version.  With Cargo.lock present,
        // member Cargo.tomls must not be scanned.
        let paths = vec![
            PathBuf::from("Cargo.lock"),
            PathBuf::from("Cargo.toml"),
            PathBuf::from("crates/a/Cargo.toml"),
            PathBuf::from("crates/b/Cargo.toml"),
        ];
        let kept = filter_redundant_manifests(paths);
        assert_eq!(kept, vec![PathBuf::from("Cargo.lock")]);
    }

    #[test]
    fn cargo_toml_kept_when_no_lockfile() {
        // If the workspace truly has no lockfile, the constraint
        // manifest is the only input we have — must still scan it
        // (and emit the warning in `append_lockfile_warnings`).
        let paths = vec![PathBuf::from("Cargo.toml")];
        let kept = filter_redundant_manifests(paths.clone());
        assert_eq!(kept, paths);
    }

    #[test]
    fn npm_lockfile_any_flavour_supersedes_package_json() {
        for lock in ["package-lock.json", "yarn.lock", "pnpm-lock.yaml"] {
            let kept = filter_redundant_manifests(vec![
                PathBuf::from(lock),
                PathBuf::from("package.json"),
                PathBuf::from("packages/foo/package.json"),
            ]);
            assert_eq!(kept, vec![PathBuf::from(lock)], "lock={lock}");
        }
    }

    #[test]
    fn exact_version_gate_rejects_constraint_strings() {
        // Anything a user would write in a Cargo.toml / package.json
        // that isn't a fully-resolved pin must be rejected so OSV
        // doesn't return "every advisory that ever mentioned this
        // package" as a match.
        for bad in [
            "", "1", "1.0", "^1.2", "~1.2.3", ">=1.0,<2.0", "1.2.*",
            "1.0 || 2.0", " ", "latest",
        ] {
            assert!(!is_exact_version(bad), "must reject: {bad:?}");
        }
        // Real lockfile-resolved versions.
        for good in [
            "1.52.1", "0.3.32", "2.0.0-alpha.1", "1.0.0+git.abcdef",
            "0.0.1", "10.0.0",
        ] {
            assert!(is_exact_version(good), "must accept: {good:?}");
        }
    }

    #[test]
    fn npm_lockfile_warning_is_single_and_satisfied_by_any_flavour() {
        // package-lock.json, yarn.lock, or pnpm-lock.yaml all
        // satisfy the "pinned somewhere" requirement — not all three.
        for lock in ["package-lock.json", "yarn.lock", "pnpm-lock.yaml"] {
            let mut r = report_with(&["package.json", lock]);
            append_lockfile_warnings(&mut r);
            assert!(
                r.warnings.is_empty(),
                "{lock} should satisfy the npm lockfile check: {:?}",
                r.warnings,
            );
        }
        let mut r = report_with(&["package.json"]);
        append_lockfile_warnings(&mut r);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("npm/yarn/pnpm lockfile"));
    }
}
