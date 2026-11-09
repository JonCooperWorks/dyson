# dyson-dependency-analysis parser

## Overview
This directory holds one manifest and lockfile parser per ecosystem. Each one implements `ManifestParser`, which turns raw file bytes into `Parsed { deps, warnings }`. Choosing a parser from the file path happens in `super::detect::parser_for`. The parsers themselves never match on filenames to decide whether they apply. They only branch on the lowercased name to pick a format variant ([mod.rs:module comment], [mod.rs:file_name_lower]).

## Interface
- `trait ManifestParser: Send + Sync { fn parse(&self, path: &Path, bytes: &[u8]) -> Result<Parsed, ParseError> }` ([mod.rs:ManifestParser]).
- Parser unit structs, one per module:
  - `CargoParser`: `Cargo.lock` and `Cargo.toml` ([cargo.rs:CargoParser]).
  - `ConanParser`: `conan.lock` ([conan.rs:ConanParser]).
  - `CranParser`: `renv.lock` and `DESCRIPTION` ([cran.rs:CranParser]).
  - `GithubActionsParser`: workflow YAML `uses:` entries ([github_actions.rs:GithubActionsParser]).
  - `GoParser`: `go.sum` and `go.mod` ([go.rs:GoParser]).
  - `HackageParser`: `cabal.project.freeze` and `stack.yaml.lock` ([hackage.rs:HackageParser]).
  - `HexParser`: `mix.lock` ([hex.rs:HexParser]).
  - `MavenParser`: `pom.xml` and `gradle.lockfile` ([maven.rs:MavenParser]).
  - `NpmParser`: `package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock` (v1 and v2) and `package.json` ([npm.rs:NpmParser]).
  - `NugetParser`: `packages.lock.json`, `packages.config` and `*.csproj`/`*.fsproj`/`*.vbproj` ([nuget.rs:NugetParser]).
  - `PackagistParser`: `composer.lock` ([packagist.rs:PackagistParser]).
  - `PubParser`: `pubspec.lock` ([pub_.rs:PubParser]).
  - `PypiParser`: `Pipfile.lock`, TOML lockfiles, `pyproject.toml` and `requirements*.txt` ([pypi.rs:PypiParser]).
  - `RubyGemsParser`: `Gemfile.lock` ([rubygems.rs:RubyGemsParser]).
  - `SbomParser`: CycloneDX and SPDX JSON ([sbom.rs:SbomParser]).
  - `SwiftParser`: `Package.resolved` ([swift.rs:SwiftParser]).
- Crate-private helpers:
  - `toml_version` ([mod.rs:toml_version]).
  - `file_name_lower` ([mod.rs:file_name_lower]).
  - `utf8` ([mod.rs:utf8]).
  - `from_json`, `from_yaml` and `from_toml`, which turn decode failures into `ParseError::malformed(path, "{label} decode: {e}")` ([mod.rs:from_json]).
  - `dep(name, version, ecosystem, source)` ([mod.rs:dep]).

## Invariants
- `dep()` defaults to `direct = false` and `purl = None`. Parsers set those fields explicitly when the manifest distinguishes them ([mod.rs:dep]).
- Files that are not UTF-8 or fail to decode return `ParseError` instead of an empty result ([mod.rs:utf8], [mod.rs:from_json]).
- `Cargo.lock` keeps only packages whose `source` starts with `registry+https://github.com/rust-lang/crates.io-index`. Git and path dependencies are dropped ([cargo.rs:parse_lock]).
- `NpmParser` returns `ParseError` "unrecognised npm manifest" for any other filename ([npm.rs:NpmParser::parse]). `PypiParser` returns "unrecognised PyPI manifest" in the same case ([pypi.rs:PypiParser::parse]).
- `package.json` ranges keep only the first version token after stripping leading `^ ~ = > <` and spaces. Ranges containing `://` or starting with `git+` or `file:` yield `None` ([npm.rs:range_to_version]).
- Constraint-only manifests push a "prefer a lockfile" style warning: `package.json` ([npm.rs:parse_package_json]), `pyproject.toml` ([pypi.rs:parse_pyproject]), `DESCRIPTION` ([cran.rs:parse_description]), and the pom/csproj paths ([maven.rs:parse_pom], [nuget.rs:parse_csproj]).
- GitHub Actions `uses:` values starting with `./` or `docker://` are skipped. `owner/repo/path@ref` collapses to `owner/repo`, and the dependency is marked `direct = true` ([github_actions.rs:split_uses], [github_actions.rs:push_uses]).
- SBOM detection treats JSON as CycloneDX when `bomFormat == "CycloneDX"` or a `components` array is present. It treats it as SPDX when `spdxVersion` is present. Anything else is a `ParseError` ([sbom.rs:SbomParser::parse]).
- SBOM components without a PURL, or with an unparseable PURL, are skipped with a warning. Unknown PURL types become `Ecosystem::Other(type)` ([sbom.rs:push_purl], [sbom.rs:split_purl]).
- `pubspec.lock` sets `direct` when the entry's `dependency` field starts with `"direct"` ([pub_.rs:PubParser::parse]).

## Side Effects
- None for I/O. Parsers work only on the bytes they are given and do no file or network access.
- Global state: lazily compiled `static OnceLock<Regex>` patterns, for example `GO_SUM_LINE` and `GO_MOD_REQUIRE` ([go.rs]), `DEPENDENCY_BLOCK` ([maven.rs]), `YARN_V1_ENTRY` ([npm.rs]), `PACKAGE_REF` ([nuget.rs]), `REQ_LINE` ([pypi.rs]), `SPEC_LINE` ([rubygems.rs]), `HEX_LINE` ([hex.rs]) and `FREEZE_LINE` ([hackage.rs]).

## Verification
- Run: `cargo test -p dyson-dependency-analysis` (from `dyson/`)
