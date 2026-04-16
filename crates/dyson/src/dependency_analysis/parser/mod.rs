// Manifest parsers — one impl per ecosystem.  Path-based dispatch lives
// in `super::detect::parser_for`; parsers never do filename matching.

use std::path::Path;

use super::types::{Dependency, Ecosystem, ParseError, Parsed};

pub mod cargo;
pub mod conan;
pub mod cran;
pub mod github_actions;
pub mod go;
pub mod hackage;
pub mod hex;
pub mod maven;
pub mod npm;
pub mod nuget;
pub mod packagist;
pub mod pub_;
pub mod pypi;
pub mod rubygems;
pub mod sbom;
pub mod swift;

pub trait ManifestParser: Send + Sync {
    fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError>;
}

/// Decode `bytes` as UTF-8 or surface a consistent `ParseError`.
pub(crate) fn utf8<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a str, ParseError> {
    std::str::from_utf8(bytes).map_err(|e| ParseError::malformed(path, format!("not UTF-8: {e}")))
}

/// Shared constructor for the ubiquitous `Dependency { … }` block.
/// Defaults `direct = false` (the safe default when the manifest
/// doesn't distinguish) and `purl = None`.  Set those fields directly
/// when needed.
pub(crate) fn dep(
    name: impl Into<String>,
    version: Option<String>,
    ecosystem: Ecosystem,
    source: &Path,
) -> Dependency {
    Dependency {
        name: name.into(),
        version,
        ecosystem,
        purl: None,
        source_file: source.to_path_buf(),
        direct: false,
    }
}
