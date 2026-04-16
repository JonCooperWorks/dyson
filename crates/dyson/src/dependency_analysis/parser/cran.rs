// CRAN / Bioconductor — renv.lock (JSON) + DESCRIPTION (control-style).

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep, utf8};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct CranParser;

impl ManifestParser for CranParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.eq_ignore_ascii_case("renv.lock") {
            parse_renv(path, bytes)
        } else {
            parse_description(path, bytes)
        }
    }
}

#[derive(Deserialize)]
struct RenvLock {
    #[serde(default, rename = "Packages")]
    packages: std::collections::BTreeMap<String, RenvPackage>,
}

#[derive(Deserialize)]
struct RenvPackage {
    #[serde(rename = "Package")]
    package: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(default, rename = "Source")]
    source: String,
}

fn parse_renv(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let doc: RenvLock = serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("renv.lock decode: {e}")))?;
    let mut parsed = Parsed::default();
    for (_, pkg) in doc.packages {
        let eco = if pkg.source.eq_ignore_ascii_case("Bioconductor") {
            Ecosystem::Bioconductor
        } else {
            Ecosystem::CRAN
        };
        parsed
            .deps
            .push(dep(pkg.package, Some(pkg.version), eco, path));
    }
    Ok(parsed)
}

fn parse_description(path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let text = utf8(path, bytes)?;
    let mut parsed = Parsed::default();
    parsed.warnings.push(format!(
        "{}: DESCRIPTION only carries constraints; prefer renv.lock",
        path.display()
    ));
    for line in text.lines() {
        let Some((field, body)) = line.split_once(':') else {
            continue;
        };
        if !matches!(field.trim(), "Imports" | "Depends" | "LinkingTo" | "Suggests") {
            continue;
        }
        for item in body.split(',') {
            let name = item.split('(').next().unwrap_or("").trim();
            if name.is_empty() || name == "R" {
                continue;
            }
            let mut d = dep(name, None, Ecosystem::CRAN, path);
            d.direct = true;
            parsed.deps.push(d);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_renv_lock() {
        let doc = br#"{
            "R": {"Version": "4.3.0"},
            "Packages": {
                "ggplot2": {"Package": "ggplot2", "Version": "3.4.2", "Source": "CRAN"},
                "BiocGenerics": {"Package": "BiocGenerics", "Version": "0.48.0", "Source": "Bioconductor"}
            }
        }"#;
        let parsed = CranParser.parse(Path::new("renv.lock"), doc).unwrap();
        let bg = parsed
            .deps
            .iter()
            .find(|d| d.name == "BiocGenerics")
            .unwrap();
        assert_eq!(bg.ecosystem, Ecosystem::Bioconductor);
    }
}
