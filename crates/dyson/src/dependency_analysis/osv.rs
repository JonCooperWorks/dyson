// OSV HTTP client — queries https://api.osv.dev.
//
// Flow: scan() collects deps → querybatch() chunks them at BATCH_SIZE
// and POSTs to /v1/querybatch (returns IDs only) → fetch_details()
// dedupes IDs and GETs /v1/vulns/{id} with bounded concurrency for
// severity/affected-ranges/references.  The shared `http::client()`
// handles pooling, timeouts, and SSRF-safe redirects; `api.osv.dev` is
// a fixed, known-safe host so no extra URL verification is needed.

use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use super::types::{Dependency, Severity, Vulnerability};

const BATCH_SIZE: usize = 300;
const DETAIL_CONCURRENCY: usize = 8;
const RETRY_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_BASE: &str = "https://api.osv.dev";

#[derive(Debug, thiserror::Error)]
pub enum OsvError {
    #[error("OSV HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OSV returned unexpected payload: {0}")]
    Unexpected(String),
}

pub struct OsvClient {
    http: reqwest::Client,
    base: String,
}

impl OsvClient {
    pub fn new() -> Self {
        Self::new_with(DEFAULT_BASE.to_string(), crate::http::client().clone())
    }

    /// Constructor used by tests to point at a mock server.
    pub fn new_with(base: String, http: reqwest::Client) -> Self {
        Self { http, base }
    }
}

impl Default for OsvClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---- wire types: only the fields we read are deserialized --------------

#[derive(Serialize)]
struct QueryBatchBody {
    queries: Vec<QueryOne>,
}

/// One OSV `queries[]` entry.  Owns its strings — a handful of clones
/// per dep, trivial against the RTT; in exchange the code is lifetime-free.
#[derive(Serialize)]
#[serde(untagged)]
enum QueryOne {
    WithPackage { package: Package, version: String },
    WithPurl { package: PackagePurl },
}

#[derive(Serialize)]
struct Package {
    name: String,
    ecosystem: String,
}

#[derive(Serialize)]
struct PackagePurl {
    purl: String,
}

#[derive(Deserialize)]
struct QueryBatchResponse {
    results: Vec<BatchResult>,
}

#[derive(Deserialize, Default)]
struct BatchResult {
    #[serde(default)]
    vulns: Vec<VulnRef>,
}

#[derive(Deserialize)]
struct VulnRef {
    id: String,
}

#[derive(Deserialize)]
struct VulnDetail {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    details: String,
    #[serde(default)]
    severity: Vec<VulnSeverity>,
    #[serde(default)]
    affected: Vec<VulnAffected>,
    #[serde(default)]
    references: Vec<VulnReference>,
}

#[derive(Deserialize)]
struct VulnSeverity {
    #[serde(rename = "type")]
    ty: String,
    score: String,
}

#[derive(Deserialize, Default)]
struct VulnAffected {
    #[serde(default)]
    ranges: Vec<VulnRange>,
}

#[derive(Deserialize, Default)]
struct VulnRange {
    #[serde(default)]
    events: Vec<VulnEvent>,
}

#[derive(Deserialize, Default)]
struct VulnEvent {
    #[serde(default)]
    introduced: Option<String>,
    #[serde(default)]
    fixed: Option<String>,
    #[serde(default)]
    last_affected: Option<String>,
}

#[derive(Deserialize)]
struct VulnReference {
    url: String,
}

// ---- public API ---------------------------------------------------------

impl OsvClient {
    /// Query OSV for every dep that has a PURL or a (name, eco, version)
    /// triple.  Returns a parallel slice of vuln IDs per input dep; the
    /// caller must pre-filter deps without a pinned version.
    pub async fn querybatch(&self, deps: &[&Dependency]) -> Result<Vec<Vec<String>>, OsvError> {
        let mut out: Vec<Vec<String>> = Vec::with_capacity(deps.len());
        for chunk in deps.chunks(BATCH_SIZE) {
            let body = QueryBatchBody {
                queries: chunk.iter().map(|d| build_query(d)).collect(),
            };
            let resp: QueryBatchResponse = self.post_with_retry("/v1/querybatch", &body).await?;
            if resp.results.len() != chunk.len() {
                return Err(OsvError::Unexpected(format!(
                    "batch result count ({}) != query count ({})",
                    resp.results.len(),
                    chunk.len()
                )));
            }
            for r in resp.results {
                out.push(r.vulns.into_iter().map(|v| v.id).collect());
            }
        }
        Ok(out)
    }

    /// Fetch full detail for a list of vuln IDs, deduping and fanning
    /// out with bounded concurrency.  IDs whose fetch fails are dropped
    /// and surfaced as warnings in the returned tuple.
    pub async fn fetch_details(&self, ids: &[String]) -> (Vec<Vulnerability>, Vec<String>) {
        let mut unique: Vec<String> = ids.to_vec();
        unique.sort();
        unique.dedup();

        let results: Vec<Result<Vulnerability, OsvError>> = stream::iter(unique)
            .map(|id| async move { self.vuln(&id).await })
            .buffer_unordered(DETAIL_CONCURRENCY)
            .collect()
            .await;

        let mut vulns = Vec::with_capacity(results.len());
        let mut warnings = Vec::new();
        for r in results {
            match r {
                Ok(v) => vulns.push(v),
                Err(e) => warnings.push(format!("OSV vuln detail fetch failed: {e}")),
            }
        }
        (vulns, warnings)
    }

    async fn vuln(&self, id: &str) -> Result<Vulnerability, OsvError> {
        let url = format!("{}/v1/vulns/{}", self.base, id);
        let resp = self.get_with_retry(&url).await?;
        let detail: VulnDetail = resp
            .json()
            .await
            .map_err(|e| OsvError::Unexpected(format!("vuln detail JSON decode: {e}")))?;
        Ok(detail_to_vuln(detail))
    }

    async fn post_with_retry<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, OsvError> {
        let url = format!("{}{}", self.base, path);
        for attempt in 0..2 {
            let resp = self.http.post(&url).json(body).send().await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.json().await?);
            }
            if attempt == 0 && is_retryable(status) {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            return Err(OsvError::Unexpected(format!(
                "POST {path} returned {status}"
            )));
        }
        unreachable!("loop always returns")
    }

    async fn get_with_retry(&self, url: &str) -> Result<reqwest::Response, OsvError> {
        for attempt in 0..2 {
            let resp = self.http.get(url).send().await?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            if attempt == 0 && is_retryable(status) {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            return Err(OsvError::Unexpected(format!("GET {url} returned {status}")));
        }
        unreachable!("loop always returns")
    }
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn build_query(dep: &Dependency) -> QueryOne {
    // PURL queries implicitly carry the ecosystem; prefer them when
    // available (SBOMs always ship them).
    if let Some(purl) = &dep.purl {
        return QueryOne::WithPurl {
            package: PackagePurl { purl: purl.clone() },
        };
    }
    QueryOne::WithPackage {
        package: Package {
            name: dep.name.clone(),
            ecosystem: dep.ecosystem.osv_id().into_owned(),
        },
        version: dep.version.clone().unwrap_or_default(),
    }
}

fn detail_to_vuln(d: VulnDetail) -> Vulnerability {
    let severity = d
        .severity
        .iter()
        .find_map(|s| parse_cvss(&s.ty, &s.score))
        .unwrap_or(Severity::Unknown);

    let mut affected_ranges = Vec::new();
    let mut fixed_versions = Vec::new();
    for a in &d.affected {
        for r in &a.ranges {
            let mut intro: Option<&str> = None;
            let mut fix: Option<&str> = None;
            let mut last: Option<&str> = None;
            for e in &r.events {
                intro = e.introduced.as_deref().or(intro);
                fix = e.fixed.as_deref().or(fix);
                last = e.last_affected.as_deref().or(last);
            }
            let label = match (intro, fix, last) {
                (Some(i), Some(f), _) => format!(">= {i}, < {f}"),
                (Some(i), None, Some(l)) => format!(">= {i}, <= {l}"),
                (Some(i), None, None) => format!(">= {i}"),
                (None, Some(f), _) => format!("< {f}"),
                _ => continue,
            };
            affected_ranges.push(label);
            if let Some(f) = fix {
                fixed_versions.push(f.to_string());
            }
        }
    }
    fixed_versions.sort();
    fixed_versions.dedup();

    let summary = if d.summary.is_empty() {
        d.details.lines().next().unwrap_or("").to_string()
    } else {
        d.summary
    };

    Vulnerability {
        id: d.id,
        aliases: d.aliases,
        summary,
        severity,
        affected_ranges,
        references: d.references.into_iter().map(|r| r.url).collect(),
        fixed_versions,
    }
}

/// OSV `severity[]` carries either a plain numeric base score or a
/// CVSS vector.  We only handle the numeric path — CVSS entries
/// recently ship with both, so this is rarely lossy.
fn parse_cvss(_ty: &str, score: &str) -> Option<Severity> {
    score.parse::<f64>().ok().map(Severity::from_cvss_score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency_analysis::types::Ecosystem;
    use std::path::PathBuf;

    fn dep(version: &str, eco: Ecosystem) -> Dependency {
        Dependency {
            name: "foo".into(),
            version: Some(version.into()),
            ecosystem: eco,
            purl: None,
            source_file: PathBuf::from("Cargo.lock"),
            direct: true,
        }
    }

    #[test]
    fn build_query_prefers_purl_when_present() {
        let mut d = dep("1.0.0", Ecosystem::CratesIo);
        d.purl = Some("pkg:cargo/foo@1.0.0".into());
        assert!(matches!(build_query(&d), QueryOne::WithPurl { .. }));
    }

    #[test]
    fn build_query_uses_name_eco_version() {
        let d = dep("1.0.0", Ecosystem::CratesIo);
        match build_query(&d) {
            QueryOne::WithPackage { package, version } => {
                assert_eq!(package.name, "foo");
                assert_eq!(package.ecosystem, "crates.io");
                assert_eq!(version, "1.0.0");
            }
            _ => panic!("expected WithPackage"),
        }
    }

    #[test]
    fn parse_cvss_numeric() {
        assert_eq!(parse_cvss("CVSS_V3", "9.8"), Some(Severity::Critical));
        assert_eq!(parse_cvss("CVSS_V3", "notanum"), None);
    }
}
