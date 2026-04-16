// Packagist (PHP) — composer.lock.

use std::path::Path;

use serde::Deserialize;

use super::{ManifestParser, dep};
use crate::dependency_analysis::types::{Ecosystem, ParseError, Parsed};

pub struct PackagistParser;

#[derive(Deserialize)]
struct ComposerLock {
    #[serde(default)]
    packages: Vec<ComposerPkg>,
    #[serde(default, rename = "packages-dev")]
    packages_dev: Vec<ComposerPkg>,
}

#[derive(Deserialize)]
struct ComposerPkg {
    name: String,
    version: String,
}

impl ManifestParser for PackagistParser {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> {
        let doc: ComposerLock = serde_json::from_slice(bytes)
            .map_err(|e| ParseError::malformed(path, format!("composer.lock decode: {e}")))?;
        let mut parsed = Parsed::default();
        for pkg in doc.packages.into_iter().chain(doc.packages_dev.into_iter()) {
            let version = pkg.version.trim_start_matches('v').to_string();
            parsed
                .deps
                .push(dep(pkg.name, Some(version), Ecosystem::Packagist, path));
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_composer_lock_and_strips_v_prefix() {
        let doc = br#"{
            "packages": [{"name": "symfony/console", "version": "v6.3.0"}],
            "packages-dev": [{"name": "phpunit/phpunit", "version": "10.0.0"}]
        }"#;
        let parsed = PackagistParser
            .parse(Path::new("composer.lock"), doc)
            .unwrap();
        let console = parsed
            .deps
            .iter()
            .find(|d| d.name == "symfony/console")
            .unwrap();
        assert_eq!(console.version.as_deref(), Some("6.3.0"));
    }
}
