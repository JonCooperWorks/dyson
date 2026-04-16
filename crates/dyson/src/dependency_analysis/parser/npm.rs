// npm / Yarn / pnpm.  Lockfiles give resolved versions; `package.json`
// is a fallback that carries ranges and warns the reader.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct NpmParser;

impl ManifestParser for NpmParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "package-lock.json" | "npm-shrinkwrap.json" => parse_package_lock(path, bytes),
            "pnpm-lock.yaml" => parse_pnpm(path, bytes),
            "yarn.lock" => parse_yarn(path, bytes),
            "package.json" => parse_package_json(path, bytes),
            _ => Err(ParseError::malformed(path, "unrecognised npm manifest")),
        }
    }
}

// ---- package-lock.json (v1/v2/v3) --------------------------------------

#[derive(Deserialize)]
struct PackageLock {
    #[serde(default)]
    packages: std::collections::BTreeMap<String, PkgEntry>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, DepEntry>,
}

#[derive(Deserialize)]
struct PkgEntry {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct DepEntry {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    dependencies: Option<std::collections::BTreeMap<String, DepEntry>>,
}

fn parse_package_lock(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: PackageLock = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("package-lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    // v2/v3: `packages` keyed by install path ("" = root).
    for (key, entry) in doc.packages {
        if key.is_empty() {
            continue;
        }
        let name = key
            .rsplit_once("node_modules/")
            .map(|(_, n)| n.to_string())
            .or(entry.name);
        let (Some(name), Some(version)) = (name, entry.version) else {
            continue;
        };
        parsed
            .deps
            .push(dep(name, Some(version), Ecosystem::Npm, path));
    }
    // v1 fallback: nested `dependencies` tree.
    walk_v1(&doc.dependencies, path, &mut parsed.deps);
    Ok(parsed)
}

fn walk_v1(
    deps: &std::collections::BTreeMap<String, DepEntry>,
    path: &Path,
    out: &mut Vec<crate::dependency_analysis::types::Dependency>,
) {
    for (name, entry) in deps {
        if let Some(v) = &entry.version {
            out.push(dep(name, Some(v.clone()), Ecosystem::Npm, path));
        }
        if let Some(inner) = &entry.dependencies {
            walk_v1(inner, path, out);
        }
    }
}

// ---- package.json --------------------------------------------------------

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: std::collections::BTreeMap<String, String>,
}

fn parse_package_json(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: PackageJson = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("package.json decode: {e}")))?;
    let mut parsed = Parsed::default();
    parsed.warnings.push(format!(
        "{}: package.json holds ranges; prefer package-lock.json",
        path.display()
    ));
    for (section, direct) in [(doc.dependencies, true), (doc.dev_dependencies, false)] {
        for (name, range) in section {
            let mut d = dep(name, range_to_version(&range), Ecosystem::Npm, path);
            d.direct = direct;
            parsed.deps.push(d);
        }
    }
    Ok(parsed)
}

fn range_to_version(r: &str) -> Option<String> {
    let r = r.trim();
    if r.contains("://") || r.starts_with("git+") || r.starts_with("file:") {
        return None;
    }
    let stripped = r.trim_start_matches(['^', '~', '=', '>', '<', ' ']);
    let first = stripped.split_whitespace().next().unwrap_or(stripped);
    (!first.is_empty()).then(|| first.to_string())
}

// ---- pnpm-lock.yaml ------------------------------------------------------

#[derive(Deserialize)]
struct PnpmLock {
    #[serde(default)]
    packages: std::collections::BTreeMap<String, serde_yaml_ng::Value>,
}

fn parse_pnpm(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: PnpmLock = serde_yaml_ng::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("pnpm-lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for key in doc.packages.keys() {
        if let Some((name, ver)) = split_pnpm_key(key) {
            parsed.deps.push(dep(name, Some(ver), Ecosystem::Npm, path));
        }
    }
    Ok(parsed)
}

fn split_pnpm_key(key: &str) -> Option<(String, String)> {
    let s = key.strip_prefix('/').unwrap_or(key);
    // v9 trailing peer suffix in parens.
    let s = s.split_once('(').map(|(pre, _)| pre).unwrap_or(s);
    // v9: '@' separator (scoped names also start with '@').
    if let Some(at) = s.rfind('@')
        && at > 0
    {
        let (n, v) = s.split_at(at);
        let v = &v[1..];
        if !v.is_empty() {
            return Some((n.to_string(), v.to_string()));
        }
    }
    // v8 fallback: '/' separator.
    let (n, v) = s.rsplit_once('/')?;
    Some((n.to_string(), v.to_string()))
}

// ---- yarn.lock v1 (text) + v2 (YAML) ------------------------------------

fn parse_yarn(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    if text.contains("__metadata:") {
        parse_yarn_v2(path, text)
    } else {
        parse_yarn_v1(path, text)
    }
}

static YARN_V1_ENTRY: OnceLock<Regex> = OnceLock::new();

fn parse_yarn_v1(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let re = YARN_V1_ENTRY.get_or_init(|| {
        Regex::new(
            r#"(?m)^"?((?:@[^"\s,@:]+/)?[^"\s,@:]+)@[^\n"]*"?(?:,\s*"?[^"\n]+"?)?:\s*\n(?:\s+[^\n]+\n)*?\s+version "([^"]+)""#,
        )
        .unwrap()
    });
    let mut parsed = Parsed::default();
    for c in re.captures_iter(text) {
        let (Some(name), Some(version)) = (c.get(1), c.get(2)) else {
            continue;
        };
        parsed.deps.push(dep(
            name.as_str(),
            Some(version.as_str().to_string()),
            Ecosystem::Npm,
            path,
        ));
    }
    Ok(parsed)
}

#[derive(Deserialize)]
struct YarnV2 {
    #[serde(flatten)]
    entries: std::collections::BTreeMap<String, YarnV2Entry>,
}

#[derive(Deserialize)]
struct YarnV2Entry {
    #[serde(default)]
    version: Option<String>,
}

fn parse_yarn_v2(path: &Path, text: &str) -> Result<Parsed, ParseError> {
    let doc: YarnV2 = serde_yaml_ng::from_str(text)
        .map_err(|e| ParseError::malformed(path, format!("yarn v2 lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for (key, entry) in doc.entries {
        if key == "__metadata" {
            continue;
        }
        let name = key
            .split(',')
            .next()
            .and_then(|s| {
                let s = s.trim().trim_matches('"');
                let at = s.rfind('@')?;
                (at > 0).then(|| s[..at].to_string())
            })
            .unwrap_or_default();
        if let Some(version) = entry.version
            && !name.is_empty()
        {
            parsed
                .deps
                .push(dep(name, Some(version), Ecosystem::Npm, path));
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_lock_v3() {
        let doc = br#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "root", "version": "0.1.0"},
                "node_modules/foo": {"version": "1.2.3"},
                "node_modules/@scope/bar": {"version": "0.0.1"}
            }
        }"#;
        let parsed = NpmParser
            .parse(Path::new("package-lock.json"), doc)
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"@scope/bar"));
    }

    #[test]
    fn parses_package_json_with_warning() {
        let doc = br#"{
            "dependencies": {"foo": "^1.2.3"},
            "devDependencies": {"bar": "~2.0.0"}
        }"#;
        let parsed = NpmParser.parse(Path::new("package.json"), doc).unwrap();
        assert_eq!(parsed.deps.len(), 2);
        assert!(!parsed.warnings.is_empty());
    }

    #[test]
    fn split_pnpm_key_v8_v9() {
        assert_eq!(
            split_pnpm_key("/foo@1.2.3"),
            Some(("foo".into(), "1.2.3".into()))
        );
        assert_eq!(
            split_pnpm_key("/@scope/bar@0.0.1"),
            Some(("@scope/bar".into(), "0.0.1".into()))
        );
        assert_eq!(
            split_pnpm_key("/foo@1.2.3(peer@4.0.0)"),
            Some(("foo".into(), "1.2.3".into()))
        );
    }

    #[test]
    fn yarn_v1_entry() {
        let text = "foo@^1.0.0, foo@~1.2:\n  version \"1.2.3\"\n\n\"@scope/bar@0.0.1\":\n  version \"0.0.1\"\n";
        let parsed = NpmParser
            .parse(Path::new("yarn.lock"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"@scope/bar"));
    }
}
