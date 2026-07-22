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

/// Pull the raw version string from a TOML dependency value: either the
/// bare `version = "1.2"` string or the `{ version = "1.2", … }` table form.
/// Callers apply their own post-processing (e.g. stripping `^`/`==`).
pub(crate) fn toml_version(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t
            .get("version")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// Lowercased file name of `path` (empty string when there is none).
/// Manifest parsers branch on this to pick a flavor; the value is owned
/// because `to_ascii_lowercase` allocates.
pub(crate) fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Decode `bytes` as UTF-8 or surface a consistent `ParseError`.
pub(crate) fn utf8<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a str, ParseError> {
    std::str::from_utf8(bytes).map_err(|e| ParseError::malformed(path, format!("not UTF-8: {e}")))
}

/// Decode JSON `bytes` into `T`, mapping a decode failure to a
/// `{label} decode: {e}` `ParseError` for consistent diagnostics.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
    label: &str,
) -> Result<T, ParseError> {
    serde_json::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("{label} decode: {e}")))
}

/// Decode YAML `bytes` into `T`, mapping a decode failure to a
/// `{label} decode: {e}` `ParseError`.
pub(crate) fn from_yaml<T: serde::de::DeserializeOwned>(
    path: &Path,
    bytes: &[u8],
    label: &str,
) -> Result<T, ParseError> {
    serde_yaml_ng::from_slice(bytes)
        .map_err(|e| ParseError::malformed(path, format!("{label} decode: {e}")))
}

/// Decode TOML `text` into `T`, mapping a decode failure to a
/// `{label} decode: {e}` `ParseError`.
pub(crate) fn from_toml<T: serde::de::DeserializeOwned>(
    path: &Path,
    text: &str,
    label: &str,
) -> Result<T, ParseError> {
    toml::from_str(text).map_err(|e| ParseError::malformed(path, format!("{label} decode: {e}")))
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
