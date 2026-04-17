// Shared types for the `dependency_analysis` module.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// OSV ecosystem identifier (https://ossf.github.io/osv-schema/).
/// `Other(String)` is the escape hatch: distros and any ecosystem a
/// parser doesn't special-case flow through here with the OSV-facing
/// string pre-formatted (e.g. `"Debian:11"` or a PURL `type`).
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize)]
pub enum Ecosystem {
    CratesIo,
    Npm,
    PyPI,
    Go,
    Maven,
    NuGet,
    RubyGems,
    Packagist,
    Pub,
    Hex,
    CRAN,
    Bioconductor,
    SwiftURL,
    GitHubActions,
    Hackage,
    ConanCenter,
    Other(String),
}

impl Ecosystem {
    /// The exact identifier OSV expects in query payloads.  Returns a
    /// borrowed `&'static str` for the common case; owned `String`
    /// only for distro variants embedding a release.
    pub fn osv_id(&self) -> Cow<'_, str> {
        match self {
            Self::CratesIo => Cow::Borrowed("crates.io"),
            Self::Npm => Cow::Borrowed("npm"),
            Self::PyPI => Cow::Borrowed("PyPI"),
            Self::Go => Cow::Borrowed("Go"),
            Self::Maven => Cow::Borrowed("Maven"),
            Self::NuGet => Cow::Borrowed("NuGet"),
            Self::RubyGems => Cow::Borrowed("RubyGems"),
            Self::Packagist => Cow::Borrowed("Packagist"),
            Self::Pub => Cow::Borrowed("Pub"),
            Self::Hex => Cow::Borrowed("Hex"),
            Self::CRAN => Cow::Borrowed("CRAN"),
            Self::Bioconductor => Cow::Borrowed("Bioconductor"),
            Self::SwiftURL => Cow::Borrowed("SwiftURL"),
            Self::GitHubActions => Cow::Borrowed("GitHub Actions"),
            Self::Hackage => Cow::Borrowed("Hackage"),
            Self::ConanCenter => Cow::Borrowed("ConanCenter"),
            Self::Other(s) => Cow::Borrowed(s.as_str()),
        }
    }

    /// Map the `type` segment of a PURL to an `Ecosystem`.  Returns
    /// `None` for types OSV doesn't recognise.
    pub fn from_purl_type(ty: &str) -> Option<Self> {
        Some(match ty {
            "cargo" => Self::CratesIo,
            "npm" => Self::Npm,
            "pypi" => Self::PyPI,
            "golang" => Self::Go,
            "maven" => Self::Maven,
            "nuget" => Self::NuGet,
            "gem" => Self::RubyGems,
            "composer" => Self::Packagist,
            "pub" => Self::Pub,
            "hex" => Self::Hex,
            "cran" => Self::CRAN,
            "bioconductor" => Self::Bioconductor,
            "swift" => Self::SwiftURL,
            "githubactions" | "github" => Self::GitHubActions,
            "hackage" => Self::Hackage,
            "conan" => Self::ConanCenter,
            _ => return None,
        })
    }

    /// Inverse of [`from_purl_type`] for emitting PURLs.  `None` for
    /// `Other(_)` since the distro string isn't a PURL type.
    pub fn to_purl_type(&self) -> Option<&'static str> {
        Some(match self {
            Self::CratesIo => "cargo",
            Self::Npm => "npm",
            Self::PyPI => "pypi",
            Self::Go => "golang",
            Self::Maven => "maven",
            Self::NuGet => "nuget",
            Self::RubyGems => "gem",
            Self::Packagist => "composer",
            Self::Pub => "pub",
            Self::Hex => "hex",
            Self::CRAN => "cran",
            Self::Bioconductor => "bioconductor",
            Self::SwiftURL => "swift",
            Self::GitHubActions => "githubactions",
            Self::Hackage => "hackage",
            Self::ConanCenter => "conan",
            Self::Other(_) => return None,
        })
    }
}

/// A resolved dependency.  `version` is `None` for un-pinned manifests;
/// such deps are skipped at OSV query time.
#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub name: String,
    pub version: Option<String>,
    pub ecosystem: Ecosystem,
    /// Preferred query shape when present (SBOMs ship PURLs directly).
    pub purl: Option<String>,
    pub source_file: PathBuf,
    /// `true` iff the manifest records this as a top-level dep.
    pub direct: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Vulnerability {
    pub id: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub severity: Severity,
    pub affected_ranges: Vec<String>,
    pub references: Vec<String>,
    pub fixed_versions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Severity {
    Unknown,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// Map a CVSS numeric base score to a coarse bucket per CVSS v3 bands.
    pub fn from_cvss_score(score: f64) -> Self {
        if score >= 9.0 {
            Self::Critical
        } else if score >= 7.0 {
            Self::High
        } else if score >= 4.0 {
            Self::Medium
        } else if score > 0.0 {
            Self::Low
        } else {
            Self::Unknown
        }
    }
}

#[derive(Default)]
pub struct Parsed {
    pub deps: Vec<Dependency>,
    pub warnings: Vec<String>,
}

/// Parser-level error for malformed files.  Format-level mismatches
/// (wrong filename) are signalled by `detect::parser_for` returning
/// `None`; I/O errors are handled by the scanner caller.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {msg}")]
pub struct ParseError {
    pub path: PathBuf,
    pub msg: String,
}

impl ParseError {
    pub fn malformed(path: &Path, msg: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            msg: msg.into(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ScanReport {
    pub scanned_files: Vec<PathBuf>,
    /// Recognised but unparseable — do not treat as clean.
    pub unsupported: Vec<PathBuf>,
    pub deps_total: usize,
    pub deps_queried: usize,
    /// Every dep parsed across all manifests, in discovery order.  Kept
    /// for SBOM emission — an SBOM lists every component, not only the
    /// vulnerable ones.  Entries here are not deduped across manifests
    /// (a dep pinned in two lockfiles appears twice).
    pub deps: Vec<Dependency>,
    pub findings: Vec<(Dependency, Vec<Vulnerability>)>,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osv_id_borrowed_and_other_passthrough() {
        let id = Ecosystem::CratesIo.osv_id();
        assert!(matches!(id, Cow::Borrowed(_)));
        assert_eq!(id.as_ref(), "crates.io");
        assert_eq!(Ecosystem::Other("Debian:11".into()).osv_id().as_ref(), "Debian:11");
    }

    #[test]
    fn from_purl_type_common() {
        assert_eq!(Ecosystem::from_purl_type("cargo"), Some(Ecosystem::CratesIo));
        assert_eq!(Ecosystem::from_purl_type("golang"), Some(Ecosystem::Go));
        assert_eq!(Ecosystem::from_purl_type("nope"), None);
    }

    #[test]
    fn severity_bands() {
        assert_eq!(Severity::from_cvss_score(9.8), Severity::Critical);
        assert_eq!(Severity::from_cvss_score(7.0), Severity::High);
        assert_eq!(Severity::from_cvss_score(5.0), Severity::Medium);
        assert_eq!(Severity::from_cvss_score(3.5), Severity::Low);
        assert_eq!(Severity::from_cvss_score(0.0), Severity::Unknown);
    }
}
