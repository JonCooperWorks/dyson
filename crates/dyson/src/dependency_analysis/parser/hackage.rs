// Hackage — cabal.project.freeze + stack.yaml.lock.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct HackageParser;

static FREEZE_LINE: OnceLock<Regex> = OnceLock::new();

impl ManifestParser for HackageParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        if path.file_name().is_some_and(|n| n == "stack.yaml.lock") {
            parse_stack_lock(path, bytes)
        } else {
            parse_freeze(path, bytes)
        }
    }
}

fn parse_freeze(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let re = FREEZE_LINE
        .get_or_init(|| Regex::new(r"any\.([A-Za-z0-9\-_]+)\s*==\s*([0-9A-Za-z.+\-]+)").unwrap());
    let mut parsed = Parsed::default();
    for c in re.captures_iter(text) {
        parsed
            .deps
            .push(dep(&c[1], Some(c[2].to_string()), Ecosystem::Hackage, path));
    }
    Ok(parsed)
}

#[derive(Deserialize)]
struct StackLock {
    #[serde(default)]
    packages: Vec<StackEntry>,
}

#[derive(Deserialize)]
struct StackEntry {
    #[serde(default)]
    original: Option<StackRef>,
    #[serde(default)]
    completed: Option<StackRef>,
}

#[derive(Deserialize)]
struct StackRef {
    #[serde(default)]
    hackage: Option<String>,
}

fn parse_stack_lock(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: StackLock = serde_yaml_ng::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("stack.yaml.lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for entry in doc.packages {
        let hackage = entry
            .completed
            .and_then(|c| c.hackage)
            .or_else(|| entry.original.and_then(|o| o.hackage));
        // "foo-1.2.3@sha256:…" → name=foo, version=1.2.3
        if let Some(h) = hackage
            && let Some((name, version)) = h.split('@').next().and_then(|b| b.rsplit_once('-'))
        {
            parsed.deps.push(dep(
                name,
                Some(version.to_string()),
                Ecosystem::Hackage,
                path,
            ));
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_freeze() {
        let text = "constraints: any.aeson ==2.1.2.1,\n             any.text ==2.0.2,";
        let parsed = HackageParser
            .parse(Path::new("cabal.project.freeze"), text.as_bytes())
            .unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"aeson"));
        assert!(names.contains(&"text"));
    }
}
