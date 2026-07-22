// Filename-based dispatch from a path to its parser.  Adding an
// ecosystem = one `match` arm plus a parser module.  Parsers never
// do filename matching themselves.

use std::path::Path;

use super::parser::{
    ManifestParser, cargo::CargoParser, conan::ConanParser, cran::CranParser,
    github_actions::GithubActionsParser, go::GoParser, hackage::HackageParser, hex::HexParser,
    maven::MavenParser, npm::NpmParser, nuget::NugetParser, packagist::PackagistParser,
    pub_::PubParser, pypi::PypiParser, rubygems::RubyGemsParser, sbom::SbomParser,
    swift::SwiftParser,
};

/// Return a parser for `path`, or `None` if the filename isn't one we
/// know about (silently skipped by the scanner — not an error).
pub fn parser_for(path: &Path) -> Option<Box<dyn ManifestParser>> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();

    // SBOM first — a repo-shipped SBOM supersedes per-ecosystem parsing.
    if lower.ends_with(".cdx.json")
        || lower.ends_with(".spdx.json")
        || lower == "bom.json"
        || lower == "sbom.json"
    {
        return Some(Box::new(SbomParser));
    }

    let by_name: Option<Box<dyn ManifestParser>> = match lower.as_str() {
        "cargo.lock" | "cargo.toml" => Some(Box::new(CargoParser)),
        "package-lock.json"
        | "npm-shrinkwrap.json"
        | "yarn.lock"
        | "pnpm-lock.yaml"
        | "package.json" => Some(Box::new(NpmParser)),
        "requirements.txt" | "pipfile.lock" | "poetry.lock" | "uv.lock" | "pdm.lock"
        | "pyproject.toml" => Some(Box::new(PypiParser)),
        "go.sum" | "go.mod" => Some(Box::new(GoParser)),
        "pom.xml" | "gradle.lockfile" => Some(Box::new(MavenParser)),
        "packages.lock.json" | "packages.config" => Some(Box::new(NugetParser)),
        "gemfile.lock" => Some(Box::new(RubyGemsParser)),
        "composer.lock" => Some(Box::new(PackagistParser)),
        "pubspec.lock" => Some(Box::new(PubParser)),
        "mix.lock" => Some(Box::new(HexParser)),
        "renv.lock" | "description" => Some(Box::new(CranParser)),
        "package.resolved" => Some(Box::new(SwiftParser)),
        "cabal.project.freeze" | "stack.yaml.lock" => Some(Box::new(HackageParser)),
        "conan.lock" => Some(Box::new(ConanParser)),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }

    // Suffix / path-based fallbacks.
    if lower.starts_with("requirements") && lower.ends_with(".txt") {
        return Some(Box::new(PypiParser));
    }
    if (lower.ends_with(".yml") || lower.ends_with(".yaml"))
        && path.components().any(|c| c.as_os_str() == ".github")
        && path.components().any(|c| c.as_os_str() == "workflows")
    {
        return Some(Box::new(GithubActionsParser));
    }
    if lower.ends_with(".csproj") || lower.ends_with(".fsproj") || lower.ends_with(".vbproj") {
        return Some(Box::new(NugetParser));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(p: &str) -> bool {
        parser_for(Path::new(p)).is_some()
    }

    #[test]
    fn detects_core_manifests() {
        for p in [
            "Cargo.lock",
            "package-lock.json",
            "pnpm-lock.yaml",
            "go.sum",
            "pom.xml",
            "Gemfile.lock",
            "composer.lock",
            "pubspec.lock",
            "mix.lock",
            "Package.resolved",
            "conan.lock",
        ] {
            assert!(has(p), "expected {p} to dispatch");
        }
    }

    #[test]
    fn detects_sbom_variants() {
        assert!(has("bom.json"));
        assert!(has("project.cdx.json"));
        assert!(has("project.spdx.json"));
    }

    #[test]
    fn detects_requirements_variants() {
        assert!(has("requirements.txt"));
        assert!(has("requirements-dev.txt"));
    }

    #[test]
    fn workflow_only_inside_github_workflows() {
        assert!(parser_for(Path::new(".github/workflows/ci.yml")).is_some());
        assert!(parser_for(Path::new(".github/workflows/deploy.yaml")).is_some());
        assert!(parser_for(Path::new("docs/ci.yml")).is_none());
    }

    #[test]
    fn detects_csproj() {
        assert!(has("App.csproj"));
        assert!(has("App.fsproj"));
    }

    #[test]
    fn unknown_returns_none() {
        assert!(!has("README.md"));
        assert!(!has("LICENSE"));
    }
}
