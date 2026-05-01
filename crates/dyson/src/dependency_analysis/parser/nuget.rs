// NuGet — packages.lock.json (deterministic) + packages.config / *.csproj
// (range-only; `$(…)` property substitutions warned and skipped).

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct NugetParser;

impl ManifestParser for NugetParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name == "packages.lock.json" {
            parse_lock(path, bytes)
        } else if name == "packages.config" {
            parse_packages_config(path, bytes)
        } else {
            parse_csproj(path, bytes)
        }
    }
}

#[derive(Deserialize)]
struct NugetLock {
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, std::collections::BTreeMap<String, LockEntry>>,
}

#[derive(Deserialize)]
struct LockEntry {
    #[serde(default)]
    resolved: Option<String>,
}

fn parse_lock(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: NugetLock = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("packages.lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for (_tfm, pkgs) in doc.dependencies {
        for (name, entry) in pkgs {
            if let Some(version) = entry.resolved {
                parsed
                    .deps
                    .push(dep(name, Some(version), Ecosystem::NuGet, path));
            }
        }
    }
    Ok(parsed)
}

static PACKAGE_REF: OnceLock<Regex> = OnceLock::new();
static PACKAGES_CONFIG: OnceLock<Regex> = OnceLock::new();

fn parse_csproj(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let re = PACKAGE_REF.get_or_init(|| {
        Regex::new(r#"<PackageReference\s+Include="([^"]+)"\s+Version="([^"]+)"\s*/?>"#).unwrap()
    });
    let mut parsed = Parsed::default();
    let mut has_placeholder = false;
    for c in re.captures_iter(text) {
        let version = c[2].to_string();
        if version.contains("$(") {
            has_placeholder = true;
            continue;
        }
        let mut d = dep(&c[1], Some(version), Ecosystem::NuGet, path);
        d.direct = true;
        parsed.deps.push(d);
    }
    if has_placeholder {
        parsed.warnings.push(format!(
            "{}: skipped property-substituted versions; prefer packages.lock.json",
            path.display()
        ));
    }
    Ok(parsed)
}

fn parse_packages_config(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let re = PACKAGES_CONFIG
        .get_or_init(|| Regex::new(r#"<package\s+id="([^"]+)"\s+version="([^"]+)""#).unwrap());
    let mut parsed = Parsed::default();
    for c in re.captures_iter(text) {
        let mut d = dep(&c[1], Some(c[2].to_string()), Ecosystem::NuGet, path);
        d.direct = true;
        parsed.deps.push(d);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_packages_lock() {
        let doc = br#"{
            "dependencies": {
                "net6.0": {
                    "Newtonsoft.Json": {"resolved": "13.0.1"},
                    "Serilog":         {"resolved": "3.0.1"}
                }
            }
        }"#;
        let parsed = NugetParser
            .parse(Path::new("packages.lock.json"), doc)
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Newtonsoft.Json"));
        assert!(names.contains(&"Serilog"));
    }

    #[test]
    fn csproj_skips_placeholder() {
        let doc = br#"<Project><ItemGroup>
  <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
  <PackageReference Include="Serilog" Version="$(SerilogVersion)" />
</ItemGroup></Project>"#;
        let parsed = NugetParser.parse(Path::new("App.csproj"), doc).unwrap();
        assert_eq!(parsed.deps.len(), 1);
        assert!(!parsed.warnings.is_empty());
    }
}
