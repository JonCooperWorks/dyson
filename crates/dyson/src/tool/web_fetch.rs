// ===========================================================================
// WebFetchTool — fetch a URL and return clean extracted text.
//
// Saves tokens compared to `curl` via bash by stripping HTML tags, scripts,
// and styles, returning only readable text content.  Uses `nanohtml2text`
// (zero dependencies) for HTML-to-text conversion.
//
// Supported content types:
//   - text/html  → stripped to plain text via nanohtml2text
//   - text/plain → returned as-is
//   - application/json → pretty-printed
//   - everything else → error (unsupported content type)
// ===========================================================================

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

/// Default maximum characters in extracted text output.
const DEFAULT_MAX_LENGTH: usize = 50_000;

/// Maximum allowed `max_length` parameter value.
const MAX_MAX_LENGTH: usize = 200_000;

/// Maximum raw response body size (2 MB).  Sized for the LLM context, not
/// arbitrary downloads — a 2 MB HTML page yields several tens of thousands
/// of tokens of extracted text, already much larger than any useful snippet
/// and enough to cover very large news articles or documentation pages.
/// Parallel web_fetch calls previously allowed 5 MB × N peaks; 2 MB keeps
/// that bounded under concurrent tool execution.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Per-request timeout — web pages should respond fast; the shared client's
/// 300 s default is for LLM streaming, not page fetches.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum URL length to prevent abuse.
const MAX_URL_LENGTH: usize = 2048;

#[cfg(test)]
type UrlPreflight = std::sync::Arc<
    dyn Fn(
            String,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = std::result::Result<
                            crate::http::ValidatedSafeUrl,
                            crate::http::UrlSafetyError,
                        >,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// WebFetchTool
// ---------------------------------------------------------------------------

pub struct WebFetchTool {
    client: reqwest::Client,
    #[cfg(test)]
    preflight: Option<UrlPreflight>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: crate::http::client().clone(),
            #[cfg(test)]
            preflight: None,
        }
    }

    async fn validate_url(
        &self,
        url: &str,
    ) -> std::result::Result<crate::http::ValidatedSafeUrl, crate::http::UrlSafetyError> {
        #[cfg(test)]
        if let Some(preflight) = &self.preflight {
            return preflight(url.to_string()).await;
        }
        crate::http::validate_url_safe(url).await
    }

    fn fetch_client(
        &self,
        verified_url: &crate::http::ValidatedSafeUrl,
    ) -> Result<reqwest::Client> {
        if host_is_ip_literal(&verified_url.url) {
            return Ok(self.client.clone());
        }
        crate::http::pinned_client_for_validated_url(verified_url)
            .map_err(|e| DysonError::tool("web_fetch", format!("build pinned client: {e}")))
    }
}

fn host_is_ip_literal(url: &reqwest::Url) -> bool {
    url.host_str()
        .map(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok()
        })
        .unwrap_or(false)
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as clean text. HTML pages are \
         stripped of tags, scripts, and styles to return only readable text. \
         Use this instead of curl when you need page content without raw HTML \
         markup — it saves tokens and is easier to work with."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (must be http:// or https://)"
                },
                "max_length": {
                    "type": "integer",
                    "description": "Maximum character length of returned content (default 50000)",
                    "minimum": 1000,
                    "maximum": 200000,
                    "default": 50000
                }
            },
            "required": ["url"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // --- Parse & validate inputs ---
        let url = input["url"].as_str().unwrap_or("").to_string();

        if url.is_empty() {
            return Ok(ToolOutput::error("url is required"));
        }

        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolOutput::error(
                "only http:// and https:// URLs are supported",
            ));
        }

        if url.len() > MAX_URL_LENGTH {
            return Ok(ToolOutput::error(format!(
                "URL too long ({} chars, max {MAX_URL_LENGTH})",
                url.len(),
            )));
        }

        let verified_url = match self.validate_url(&url).await {
            Ok(verified_url) => verified_url,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        let max_length = input["max_length"]
            .as_u64()
            .unwrap_or(DEFAULT_MAX_LENGTH as u64)
            .clamp(1000, MAX_MAX_LENGTH as u64) as usize;

        tracing::info!(url = url.as_str(), "web_fetch");
        let fetch_client = self.fetch_client(&verified_url)?;

        // --- Fetch with timeout, race against cancellation ---
        let mut response = tokio::select! {
            res = fetch_client.get(verified_url.url.clone()).timeout(FETCH_TIMEOUT).send() => {
                res.map_err(|e| DysonError::tool("web_fetch", format!("request failed: {e}")))?
            }
            _ = ctx.cancellation.cancelled() => {
                return Ok(ToolOutput::error("fetch cancelled"));
            }
        };

        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutput::error(format!("HTTP {status} fetching {url}",)));
        }

        // --- Extract content type before consuming body ---
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        // --- Read body with streaming size limit ---
        // Check Content-Length header for an early reject before downloading.
        if let Some(cl) = response.content_length()
            && cl > MAX_BODY_BYTES as u64
        {
            return Ok(ToolOutput::error(format!(
                "response too large ({cl} bytes per Content-Length, max {MAX_BODY_BYTES}). \
                 Try a more specific URL.",
            )));
        }

        // Stream the body in chunks, enforcing the size limit incrementally
        // so we never buffer more than MAX_BODY_BYTES in memory.
        let mut body_bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| DysonError::tool("web_fetch", format!("failed to read body: {e}")))?
        {
            body_bytes.extend_from_slice(&chunk);
            if body_bytes.len() > MAX_BODY_BYTES {
                return Ok(ToolOutput::error(format!(
                    "response too large (>{MAX_BODY_BYTES} bytes). Try a more specific URL.",
                )));
            }
        }

        let body_str = String::from_utf8_lossy(&body_bytes);

        // --- Route on content type ---
        let text = if content_type.contains("text/html") {
            nanohtml2text::html2text(&body_str)
        } else if content_type.contains("application/json") {
            // Try to pretty-print; fall back to raw.
            match serde_json::from_str::<serde_json::Value>(&body_str) {
                Ok(val) => {
                    serde_json::to_string_pretty(&val).unwrap_or_else(|_| body_str.into_owned())
                }
                Err(_) => body_str.into_owned(),
            }
        } else if content_type.contains("text/") {
            body_str.into_owned()
        } else if content_type.is_empty() {
            // No content type header — treat as plain text.
            body_str.into_owned()
        } else {
            return Ok(ToolOutput::error(format!(
                "unsupported content type: {content_type}. \
                 This tool handles text/html, text/plain, and application/json.",
            )));
        };

        // --- Truncate ---
        let text = super::truncate(&text, max_length);
        let char_count = text.len();

        let output = format!("Content from {url} ({char_count} chars):\n\n{text}");
        Ok(ToolOutput::success(output))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tool() -> WebFetchTool {
        WebFetchTool::default()
    }

    fn preflight_to(addr: SocketAddr) -> UrlPreflight {
        Arc::new(move |url: String| {
            Box::pin(async move {
                let parsed = reqwest::Url::parse(&url)
                    .map_err(|e| crate::http::UrlSafetyError::InvalidUrl(e.to_string()))?;
                Ok(crate::http::ValidatedSafeUrl {
                    url: parsed,
                    resolved_addrs: vec![addr],
                })
            })
        })
    }

    #[tokio::test]
    async fn empty_url_returns_error() {
        let t = tool();
        let ctx = ToolContext::from_cwd().unwrap();
        let result = t.run(&json!({"url": ""}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }

    #[tokio::test]
    async fn file_url_rejected() {
        let t = tool();
        let ctx = ToolContext::from_cwd().unwrap();
        let result = t
            .run(&json!({"url": "file:///etc/passwd"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("http://"));
    }

    #[tokio::test]
    async fn dns_rebind_does_not_dial_second_resolution() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rebound_addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = hits.clone();
        let server = tokio::spawn(async move {
            if let Ok((mut stream, _peer)) = listener.accept().await {
                server_hits.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Type: text/plain\r\n\
                          Content-Length: 7\r\n\
                          Connection: close\r\n\r\n\
                          rebound",
                    )
                    .await;
            }
        });

        crate::http::ensure_crypto_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs("dns.google", &[rebound_addr])
            .build()
            .unwrap();
        let first_resolution =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), rebound_addr.port());
        let t = WebFetchTool {
            client,
            preflight: Some(preflight_to(first_resolution)),
        };
        let ctx = ToolContext::from_cwd().unwrap();
        let url = format!("http://dns.google:{}/", rebound_addr.port());

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            t.run(&json!({ "url": url }), &ctx),
        )
        .await;
        server.abort();

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "D7 web_fetch must pin the first DNS result and never dial a rebound loopback address"
        );
        if let Ok(Ok(output)) = result {
            assert!(
                output.is_error,
                "D7 web_fetch must not return content fetched from the rebound address"
            );
        }
    }

    #[tokio::test]
    async fn url_too_long() {
        let t = tool();
        let ctx = ToolContext::from_cwd().unwrap();
        let long_url = format!("https://example.com/{}", "a".repeat(MAX_URL_LENGTH));
        let result = t.run(&json!({"url": long_url}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("too long"));
    }

    #[test]
    fn html_extraction() {
        let html = "<html><head><title>Test</title><script>var x=1;</script>\
                     <style>body{color:red}</style></head>\
                     <body><h1>Hello</h1><p>World</p></body></html>";
        let text = nanohtml2text::html2text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("var x=1"));
    }

    #[test]
    fn schema_has_required_url() {
        let t = tool();
        let schema = t.input_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "url"));
    }
}
