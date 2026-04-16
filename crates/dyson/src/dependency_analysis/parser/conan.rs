// Conan (C/C++) — conan.lock.  Refs are `name/version#revision`.

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct ConanParser;

#[derive(Deserialize)]
struct ConanLock {
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    build_requires: Vec<String>,
}

impl ManifestParser for ConanParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let doc: ConanLock = serde_json::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("conan.lock decode: {e}")))?;
        let mut parsed = Parsed::default();
        for (entry, direct) in doc
            .requires
            .into_iter()
            .map(|r| (r, true))
            .chain(doc.build_requires.into_iter().map(|r| (r, false)))
        {
            let base = entry.split('#').next().unwrap_or(&entry);
            if let Some((name, version)) = base.split_once('/') {
                let mut d = dep(
                    name,
                    Some(version.to_string()),
                    Ecosystem::ConanCenter,
                    path,
                );
                d.direct = direct;
                parsed.deps.push(d);
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conan_lock() {
        let doc = br#"{
            "version": "0.5",
            "requires": ["openssl/3.2.0#rev1", "zlib/1.3"],
            "build_requires": []
        }"#;
        let parsed = ConanParser.parse(Path::new("conan.lock"), doc).unwrap();
        let names: Vec<&str> = parsed.deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"openssl"));
        assert!(names.contains(&"zlib"));
    }
}
