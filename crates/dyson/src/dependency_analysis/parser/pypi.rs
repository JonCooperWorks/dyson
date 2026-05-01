// PyPI — requirements*.txt, Pipfile.lock, poetry.lock, uv.lock, pdm.lock,
// pyproject.toml.  Lockfiles carry resolved versions; the others are
// best-effort and warn the reader.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct PypiParser;

impl ManifestParser for PypiParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "pipfile.lock" => parse_pipfile_lock(path, bytes),
            "poetry.lock" | "uv.lock" | "pdm.lock" => parse_toml_lock(path, bytes),
            "pyproject.toml" => parse_pyproject(path, bytes),
            _ if name.starts_with("requirements") && name.ends_with(".txt") => {
                parse_requirements(path, bytes)
            }
            _ => Err(ParseError::malformed(path, "unrecognised PyPI manifest")),
        }
    }
}

// ---- Pipfile.lock --------------------------------------------------------

#[derive(Deserialize)]
struct PipfileLock {
    #[serde(default)]
    default: std::collections::BTreeMap<String, PipEntry>,
    #[serde(default)]
    develop: std::collections::BTreeMap<String, PipEntry>,
}

#[derive(Deserialize)]
struct PipEntry {
    #[serde(default)]
    version: Option<String>,
}

fn parse_pipfile_lock(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: PipfileLock = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("Pipfile.lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for (name, entry) in doc.default.into_iter().chain(doc.develop.into_iter()) {
        let version = entry
            .version
            .map(|v| v.trim_start_matches("==").to_string());
        parsed.deps.push(dep(name, version, Ecosystem::PyPI, path));
    }
    Ok(parsed)
}

// ---- poetry.lock / uv.lock / pdm.lock -----------------------------------

#[derive(Deserialize)]
struct TomlLock {
    #[serde(default)]
    package: Vec<TomlLockPkg>,
}

#[derive(Deserialize)]
struct TomlLockPkg {
    name: String,
    version: String,
}

fn parse_toml_lock(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let lock: TomlLock = toml::from_str(text)
        .map_err(|e| ParseError::malformed(path, format!("TOML lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for pkg in lock.package {
        parsed
            .deps
            .push(dep(pkg.name, Some(pkg.version), Ecosystem::PyPI, path));
    }
    Ok(parsed)
}

// ---- pyproject.toml (best-effort) ---------------------------------------

fn parse_pyproject(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let doc: toml::Value = toml::from_str(text)
        .map_err(|e| ParseError::malformed(path, format!("pyproject decode: {e}")))?;
    let mut parsed = Parsed::default();
    parsed.warnings.push(format!(
        "{}: pyproject.toml holds constraints; prefer a lockfile",
        path.display()
    ));

    // PEP 621: project.dependencies is an array of PEP 508 strings.
    if let Some(arr) = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for entry in arr {
            if let Some(s) = entry.as_str()
                && let Some((name, ver)) = parse_pep508(s)
            {
                let mut d = dep(name, ver, Ecosystem::PyPI, path);
                d.direct = true;
                parsed.deps.push(d);
            }
        }
    }

    // Poetry-style: tool.poetry.dependencies is a table.
    if let Some(tbl) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (name, val) in tbl {
            if name == "python" {
                continue;
            }
            let version = match val {
                toml::Value::String(s) => Some(strip_constraint(s)),
                toml::Value::Table(t) => t
                    .get("version")
                    .and_then(|x| x.as_str())
                    .map(strip_constraint),
                _ => None,
            };
            let mut d = dep(name.clone(), version, Ecosystem::PyPI, path);
            d.direct = true;
            parsed.deps.push(d);
        }
    }
    Ok(parsed)
}

fn strip_constraint(s: &str) -> String {
    s.trim_start_matches(|c: char| "^~><=! ".contains(c))
        .to_string()
}

// ---- requirements*.txt ---------------------------------------------------

static REQ_LINE: OnceLock<Regex> = OnceLock::new();

fn parse_requirements(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let re = REQ_LINE
        .get_or_init(|| Regex::new(r"^\s*([A-Za-z0-9_.\-]+)\s*==\s*([A-Za-z0-9_.+\-]+)").unwrap());
    let mut parsed = Parsed::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        if let Some(c) = re.captures(line) {
            let mut d = dep(&c[1], Some(c[2].to_string()), Ecosystem::PyPI, path);
            d.direct = true;
            parsed.deps.push(d);
        } else {
            parsed
                .warnings
                .push(format!("{}: skipping non-pinned {line:?}", path.display()));
        }
    }
    Ok(parsed)
}

fn parse_pep508(s: &str) -> Option<(String, Option<String>)> {
    let s = s.split(';').next()?.trim();
    let bytes = s.as_bytes();
    let end = bytes
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'[')));
    let (name, rest) = end.map_or((s, ""), |i| (&s[..i], &s[i..]));
    let name = name.split('[').next().unwrap_or(name).trim().to_string();
    if name.is_empty() {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Some((name, None));
    }
    let ver = rest
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()
        .map(str::to_string)
        .filter(|v| !v.is_empty());
    Some((name, ver))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_poetry_lock() {
        let text = "[[package]]\nname = \"django\"\nversion = \"4.2.0\"\n";
        let parsed = PypiParser
            .parse(Path::new("poetry.lock"), text.as_bytes())
            .unwrap();
        assert_eq!(parsed.deps[0].name, "django");
        assert_eq!(parsed.deps[0].version.as_deref(), Some("4.2.0"));
    }

    #[test]
    fn parses_requirements_with_pin() {
        let text = "django==4.2.0\nflask==2.3.0  # note\n-r other.txt\n";
        let parsed = PypiParser
            .parse(Path::new("requirements.txt"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"django"));
        assert!(names.contains(&"flask"));
    }

    #[test]
    fn parses_pipfile_lock() {
        let doc = br#"{
            "default": {"django": {"version": "==4.2.0"}},
            "develop": {"pytest": {"version": "==8.0.0"}}
        }"#;
        let parsed = PypiParser.parse(Path::new("Pipfile.lock"), doc).unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"django"));
        assert!(names.contains(&"pytest"));
    }

    #[test]
    fn pep508_shapes() {
        assert_eq!(
            parse_pep508("django == 4.2.0"),
            Some(("django".into(), Some("4.2.0".into())))
        );
        assert_eq!(
            parse_pep508("flask[async]>=2.0"),
            Some(("flask".into(), Some("2.0".into())))
        );
        assert_eq!(parse_pep508("requests"), Some(("requests".into(), None)));
    }
}
