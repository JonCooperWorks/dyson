# dyson-dependency-analysis src

## Overview
This crate scans a file or directory for dependency manifests and lockfiles, parses them into `Dependency` records, queries the OSV vulnerability database, and returns a `ScanReport`. The modules are `detect` (filename to parser), `parser` (per-ecosystem parsers), `osv` (HTTP client) and `types` (the data model) ([lib.rs]).

## Interface
- Modules: `pub mod detect`, `pub mod osv`, `pub mod parser`, `pub mod types` ([lib.rs]).
- `parser`: the per-ecosystem `ManifestParser` implementations. See [parser/EGM.md](./parser/EGM.md).
- Re-exports: `OsvClient`, `OsvError`, `Dependency`, `Ecosystem`, `ParseError`, `Parsed`, `ScanReport`, `Severity` and `Vulnerability` ([lib.rs:pub use]).
- `ScanOptions { recursive, max_files, severity_min }`: defaults are `true`, `200` and `Severity::Unknown` ([lib.rs:ScanOptions]).
- `async fn scan(root, opts, client) -> Result<ScanReport>` ([lib.rs:scan]).
- `detect::parser_for(path) -> Option<Box<dyn ManifestParser>>` ([detect.rs:parser_for]).
- `OsvClient` constructors and methods ([osv.rs:OsvClient]):
  - `new()`, `with_http(reqwest::Client)` and `new_with(base, http)`. `Default` is also implemented.
  - `querybatch(&[&Dependency]) -> Result<Vec<Vec<String>>, OsvError>`.
  - `fetch_details(&[String]) -> (Vec<Vulnerability>, Vec<String>)`.
- `OsvError::{Http, Unexpected}` ([osv.rs:OsvError]).
- `Ecosystem` (16 named ecosystems plus `Other(String)`) with `osv_id()`, `from_purl_type()` and `to_purl_type()` ([types.rs:Ecosystem]).
- `Severity::{Unknown, Low, Medium, High, Critical}` (ordered) with `as_str()` and `from_cvss_score()` ([types.rs:Severity]).
- Data structs:
  - `Dependency { name, version, ecosystem, purl, source_file, direct }` ([types.rs:Dependency]).
  - `Vulnerability { id, aliases, summary, severity, affected_ranges, references, fixed_versions }` ([types.rs:Vulnerability]).
  - `Parsed`, `ParseError { path, msg }` with `ParseError::malformed` ([types.rs:ParseError]).
  - `ScanReport { scanned_files, unsupported, deps_total, deps_queried, deps, findings, warnings }` ([types.rs:ScanReport]).

## Invariants
- `scan` returns `Err` only when `stat` on `root` fails, as `DysonError::tool("dependency_scan", ...)`. Per-file read and parse failures go into `warnings`, and parse failures also go into `unsupported` ([lib.rs:scan], [lib.rs:discover]).
- An OSV `querybatch` failure is recorded as a warning, and the report returns without findings ([lib.rs:scan]).
- Discovery uses `ignore::WalkBuilder` with `hidden(false)` so `.github/workflows` is visited. `max_depth(1)` applies when `recursive` is false. Discovery stops after `max_files` matches ([lib.rs:discover]).
- `Cargo.toml` is skipped when a `Cargo.lock` was also discovered. `package.json` is skipped when `package-lock.json`, `yarn.lock` or `pnpm-lock.yaml` was also discovered ([lib.rs:filter_redundant_manifests]).
- At most one missing-lockfile warning is emitted per ecosystem, for Cargo and for npm/yarn/pnpm ([lib.rs:append_lockfile_warnings]).
- Only dependencies with a PURL, or with an exact `X.Y.Z[-pre][+build]` version, are sent to OSV. Range characters `^ ~ * < > = | , space` disqualify a version ([lib.rs:is_exact_version]).
- Findings drop vulnerabilities below `severity_min`. Within a finding they are sorted by severity descending, then id ([lib.rs:scan]).
- OSV limits and errors:
  - `BATCH_SIZE = 300` queries per POST, `DETAIL_CONCURRENCY = 8` detail fetches, and one retry after `RETRY_DELAY = 500ms` on a 429 or 5xx response ([osv.rs:BATCH_SIZE], [osv.rs:send_with_retry]).
  - A batch result count that differs from the query count is `OsvError::Unexpected` ([osv.rs:querybatch]).
- `fetch_details` sorts and dedups ids. Failed fetches become warnings and are dropped ([osv.rs:fetch_details]).
- Severity comes from the first numeric CVSS score in OSV's `severity[]`. Vector strings are skipped ([osv.rs:parse_cvss]). The CVSS bands are 9.0 and up Critical, 7.0 and up High, 4.0 and up Medium, above 0 Low ([types.rs:Severity::from_cvss_score]).
- `parser_for` checks SBOM names first (`*.cdx.json`, `*.spdx.json`, `bom.json`, `sbom.json`), then an exact lowercase filename table, then the `requirements*.txt` pattern, `.github/workflows/*.y(a)ml`, and `*.csproj`/`*.fsproj`/`*.vbproj` ([detect.rs:parser_for]).

## Side Effects
- Network: HTTPS calls to `https://api.osv.dev` (`DEFAULT_BASE`), `POST /v1/querybatch` and `GET /v1/vulns/{id}` ([osv.rs:DEFAULT_BASE], [osv.rs:querybatch], [osv.rs:vuln]).
- Disk reads: `std::fs::metadata(root)`, the directory walk, and `tokio::fs::read` of each manifest ([lib.rs:discover], [lib.rs:scan]).
- Timers: `tokio::time::sleep(RETRY_DELAY)` between retries ([osv.rs:send_with_retry]).

## Verification
- Run: `cargo test -p dyson-dependency-analysis` (from `dyson/`)
