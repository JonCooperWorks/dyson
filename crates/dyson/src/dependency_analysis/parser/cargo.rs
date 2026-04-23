// Cargo: Cargo.lock (resolved) + Cargo.toml (constraints-only fallback).

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct CargoParser;

impl ManifestParser for CargoParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let text = utf8(path, bytes)?;
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.eq_ignore_ascii_case("Cargo.lock") {
            parse_lock(path, text)
        } else {
            parse_toml(path, text)
        }
    }
}

#[derive(Deserialize)]
struct Lock {
    #[serde(default)]
    package: Vec<LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
}

fn parse_lock(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let lock: Lock = toml::from_str(text)
        .map_err(|e| ParseError::malformed(path, format!("Cargo.lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for pkg in lock.package {
        // git/path deps have no OSV record under the crates.io ecosystem.
        let is_crates_io = pkg.source.as_deref().is_some_and(|s| {
            s.starts_with("registry+https://github.com/rust-lang/crates.io-index")
        });
        if is_crates_io {
            parsed
                .deps
                .push(dep(pkg.name, Some(pkg.version), Ecosystem::CratesIo, path));
        }
    }
    Ok(parsed)
}

#[derive(Deserialize)]
struct CargoToml {
    #[serde(default)]
    dependencies: toml::Table,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: toml::Table,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: toml::Table,
}

fn parse_toml(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let manifest: CargoToml = toml::from_str(text)
        .map_err(|e| ParseError::malformed(path, format!("Cargo.toml decode: {e}")))?;
    let mut parsed = Parsed::default();
    // Whether to warn about the missing lockfile is decided at the
    // scan-orchestrator level in `dependency_analysis::scan` — a
    // workspace's `Cargo.lock` sits at the root and every member's
    // `Cargo.toml` resolves through it, so a per-manifest warning
    // here fires spuriously on every member ("no Cargo.lock present
    // in any crate" in a repo that does have one at the root).
    for (section, direct) in [
        (manifest.dependencies, true),
        (manifest.dev_dependencies, false),
        (manifest.build_dependencies, false),
    ] {
        for (name, val) in section {
            let mut d = dep(name, extract_version(&val), Ecosystem::CratesIo, path);
            d.direct = direct;
            parsed.deps.push(d);
        }
    }
    Ok(parsed)
}

fn extract_version(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t
            .get("version")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_deps_from_cargo_lock() {
        let text = r#"
[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name = "git-dep"
version = "0.2.0"
source = "git+https://example.com/foo.git"
"#;
        let parsed = CargoParser
            .parse(Path::new("Cargo.lock"), text.as_bytes())
            .unwrap();
        assert_eq!(parsed.deps.len(), 1);
        assert_eq!(parsed.deps[0].name, "serde");
        assert_eq!(parsed.deps[0].version.as_deref(), Some("1.0.210"));
    }

    #[test]
    fn parses_cargo_toml_sections() {
        let text = r#"
[dependencies]
serde = "1"
tokio = { version = "1.40", features = ["full"] }
[dev-dependencies]
proptest = "1"
"#;
        let parsed = CargoParser
            .parse(Path::new("Cargo.toml"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"serde"));
        assert!(names.contains(&"tokio"));
        assert!(names.contains(&"proptest"));
        // Per-manifest warnings used to fire here ("no Cargo.lock
        // present; versions may not be exact") even when the
        // workspace root did carry one.  The decision moved to the
        // scan orchestrator, which can see every file it scanned.
        assert!(
            parsed.warnings.is_empty(),
            "Cargo.toml parser no longer emits per-file lockfile warnings; \
             got: {:?}",
            parsed.warnings,
        );
    }
}
