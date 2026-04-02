// ===========================================================================
// WebSearchTool — web search with pluggable search backends.
//
// Architecture:
//   WebSearchTool (implements Tool)
//     └── Arc<dyn SearchProvider> (trait)
//           ├── BraveSearchProvider
//           ├── SearxngSearchProvider
//           └── TavilySearchProvider (future)
//
// The Tool handles input parsing, output formatting, and cancellation.
// The SearchProvider trait handles the HTTP call and response parsing.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

// ---------------------------------------------------------------------------
// SearchProvider trait — pluggable search backend.
// ---------------------------------------------------------------------------

/// A single search result returned by a provider.
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A pluggable web search backend.
///
/// Implementations handle the HTTP call and response parsing for a
/// specific search API.  The `WebSearchTool` delegates to this trait
/// and handles input/output formatting.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Execute a search query, returning up to `num_results` results.
    async fn search(&self, query: &str, num_results: usize) -> Result<Vec<SearchResult>>;
}

// ---------------------------------------------------------------------------
// BraveSearchProvider
// ---------------------------------------------------------------------------

/// Brave Search API provider.
///
/// Uses the Brave Web Search API v1:
/// `GET https://api.search.brave.com/res/v1/web/search`
///
/// Free tier: 1 query/sec, 2000 queries/month.
pub struct BraveSearchProvider {
    client: reqwest::Client,
    api_key: crate::auth::Credential,
}

impl BraveSearchProvider {
    pub fn new(api_key: crate::auth::Credential) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
        }
    }
}

#[async_trait]
impl SearchProvider for BraveSearchProvider {
    async fn search(&self, query: &str, num_results: usize) -> Result<Vec<SearchResult>> {
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", query), ("count", &num_results.to_string())])
            .header("X-Subscription-Token", self.api_key.expose())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| DysonError::tool("web_search", format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::tool(
                "web_search",
                format!("Brave API returned {status}: {body}"),
            ));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| DysonError::tool("web_search", format!("invalid JSON response: {e}")))?;

        let results = body["web"]["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| {
                        let snippet = r["description"].as_str().unwrap_or("").to_string();
                        // Truncate long snippets to keep output compact.
                        let snippet = truncate(&snippet, 200);
                        SearchResult {
                            title: r["title"].as_str().unwrap_or("").to_string(),
                            url: r["url"].as_str().unwrap_or("").to_string(),
                            snippet,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// SearxngSearchProvider
// ---------------------------------------------------------------------------

/// SearXNG search provider.
///
/// Uses the SearXNG JSON API: `GET {base_url}/search?q={query}&format=json`
///
/// No API key required for public instances.  Find public instances at
/// https://searx.space/.  Also works with self-hosted instances.
pub struct SearxngSearchProvider {
    client: reqwest::Client,
    base_url: String,
}

impl SearxngSearchProvider {
    pub fn new(base_url: String) -> Self {
        // Strip trailing slash for consistent URL construction.
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }
}

#[async_trait]
impl SearchProvider for SearxngSearchProvider {
    async fn search(&self, query: &str, num_results: usize) -> Result<Vec<SearchResult>> {
        let url = format!("{}/search", self.base_url);
        let resp = self
            .client
            .get(&url)
            .query(&[("q", query), ("format", "json"), ("pageno", "1")])
            .send()
            .await
            .map_err(|e| DysonError::tool("web_search", format!("request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::tool(
                "web_search",
                format!("SearXNG returned {status}: {body}"),
            ));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| DysonError::tool("web_search", format!("invalid JSON response: {e}")))?;

        let results = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .take(num_results)
                    .map(|r| {
                        let snippet = r["content"].as_str().unwrap_or("").to_string();
                        let snippet = truncate(&snippet, 200);
                        SearchResult {
                            title: r["title"].as_str().unwrap_or("").to_string(),
                            url: r["url"].as_str().unwrap_or("").to_string(),
                            snippet,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Factory — create a SearchProvider from config.
// ---------------------------------------------------------------------------

/// Create a `SearchProvider` from the web search configuration.
///
/// Supported providers:
/// - `"brave"` — Brave Search API (requires `api_key`)
/// - `"searxng"` — SearXNG instance (requires `base_url`, no API key needed)
///
/// Future providers can be added with a new match arm.
pub fn create_provider(config: &crate::config::WebSearchConfig) -> Result<Arc<dyn SearchProvider>> {
    match config.provider.as_str() {
        "brave" => Ok(Arc::new(BraveSearchProvider::new(config.api_key.clone()))),
        "searxng" => {
            let base_url = config.base_url.clone().ok_or_else(|| {
                DysonError::Config(
                    "searxng provider requires a base_url (e.g. \"https://searx.be\")".into(),
                )
            })?;
            Ok(Arc::new(SearxngSearchProvider::new(base_url)))
        }
        other => Err(DysonError::Config(format!(
            "unknown web search provider: \"{other}\" (supported: brave, searxng)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// WebSearchTool — the Tool implementation.
// ---------------------------------------------------------------------------

/// Built-in tool that performs web searches via a pluggable provider.
pub struct WebSearchTool {
    provider: Arc<dyn SearchProvider>,
}

impl WebSearchTool {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Use this when you need up-to-date \
         information, facts, documentation, or anything not in your training data."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "num_results": {
                    "type": "integer",
                    "description": "Number of results to return (1-10, default 5)",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let query = input["query"].as_str().unwrap_or("").to_string();

        if query.is_empty() {
            return Ok(ToolOutput::error("query is required"));
        }

        // Limit query length to prevent data exfiltration via search queries.
        const MAX_QUERY_LEN: usize = 500;
        if query.len() > MAX_QUERY_LEN {
            return Ok(ToolOutput::error(format!(
                "query too long ({} chars, max {MAX_QUERY_LEN}). Shorten the query.",
                query.len(),
            )));
        }

        tracing::info!(query = query.as_str(), "web search query");

        let num_results = input["num_results"].as_u64().unwrap_or(5).clamp(1, 10) as usize;

        // Race the search against cancellation (Ctrl-C).
        let results = tokio::select! {
            res = self.provider.search(&query, num_results) => res?,
            _ = ctx.cancellation.cancelled() => {
                return Ok(ToolOutput::error("search cancelled"));
            }
        };

        if results.is_empty() {
            return Ok(ToolOutput::success(format!(
                "No results found for: \"{query}\""
            )));
        }

        let mut output = format!("Found {} result(s) for \"{}\":\n", results.len(), query);
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!(
                "\n### {}. {}\n{}\n{}\n",
                i + 1,
                r.title,
                r.url,
                r.snippet,
            ));
        }

        Ok(ToolOutput::success(output))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a string to `max_chars`, appending "..." if truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        // Find a char boundary near max_chars.
        let mut end = max_chars;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    /// A mock search provider for testing.
    struct MockSearchProvider {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl SearchProvider for MockSearchProvider {
        async fn search(&self, _query: &str, num_results: usize) -> Result<Vec<SearchResult>> {
            Ok(self
                .results
                .iter()
                .take(num_results)
                .map(|r| SearchResult {
                    title: r.title.clone(),
                    url: r.url.clone(),
                    snippet: r.snippet.clone(),
                })
                .collect())
        }
    }

    fn mock_tool(results: Vec<SearchResult>) -> WebSearchTool {
        WebSearchTool::new(Arc::new(MockSearchProvider { results }))
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let tool = mock_tool(vec![]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool.run(json!({"query": ""}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }

    #[tokio::test]
    async fn no_results_returns_message() {
        let tool = mock_tool(vec![]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool
            .run(json!({"query": "nonexistent"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("No results found"));
    }

    #[tokio::test]
    async fn formats_results() {
        let tool = mock_tool(vec![
            SearchResult {
                title: "Rust Lang".into(),
                url: "https://rust-lang.org".into(),
                snippet: "A systems programming language".into(),
            },
            SearchResult {
                title: "Tokio".into(),
                url: "https://tokio.rs".into(),
                snippet: "Async runtime for Rust".into(),
            },
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool.run(json!({"query": "rust"}), &ctx).await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("### 1. Rust Lang"));
        assert!(result.content.contains("### 2. Tokio"));
        assert!(result.content.contains("https://rust-lang.org"));
        assert!(result.content.contains("Found 2 result(s)"));
    }

    #[tokio::test]
    async fn respects_num_results() {
        let tool = mock_tool(vec![
            SearchResult {
                title: "A".into(),
                url: "a".into(),
                snippet: "a".into(),
            },
            SearchResult {
                title: "B".into(),
                url: "b".into(),
                snippet: "b".into(),
            },
            SearchResult {
                title: "C".into(),
                url: "c".into(),
                snippet: "c".into(),
            },
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool
            .run(json!({"query": "test", "num_results": 2}), &ctx)
            .await
            .unwrap();
        assert!(result.content.contains("Found 2 result(s)"));
        assert!(!result.content.contains("### 3."));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let long = "a".repeat(300);
        let result = truncate(&long, 200);
        assert_eq!(result.len(), 203); // 200 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn tool_schema_has_required_query() {
        let tool = mock_tool(vec![]);
        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }
}
