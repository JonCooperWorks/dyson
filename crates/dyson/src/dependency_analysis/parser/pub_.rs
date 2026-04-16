// Pub (Dart / Flutter) — pubspec.lock.  Path/git/sdk sources are
// skipped because OSV has no corresponding records.

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct PubParser;

#[derive(Deserialize)]
struct PubLock {
    #[serde(default)]
    packages: std::collections::BTreeMap<String, PubEntry>,
}

#[derive(Deserialize)]
struct PubEntry {
    version: Option<String>,
    #[serde(default)]
    dependency: String,
    #[serde(default)]
    source: String,
}

impl ManifestParser for PubParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let doc: PubLock = serde_yaml_ng::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("pubspec.lock decode: {e}")))?;
        let mut parsed = Parsed::default();
        for (name, entry) in doc.packages {
            if entry.source != "hosted" && !entry.source.is_empty() {
                continue;
            }
            let mut d = dep(name, entry.version, Ecosystem::Pub, path);
            d.direct = entry.dependency.starts_with("direct");
            parsed.deps.push(d);
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosted_and_skips_path_sourced() {
        let doc = br#"
packages:
  http:
    dependency: "direct main"
    source: hosted
    version: "0.13.5"
  local_pkg:
    dependency: "direct main"
    source: path
    version: "1.0.0"
"#;
        let parsed = PubParser.parse(Path::new("pubspec.lock"), doc).unwrap();
        assert_eq!(parsed.deps.len(), 1);
        assert!(parsed.deps[0].direct);
    }
}
