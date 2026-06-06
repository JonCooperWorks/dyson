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
//   - application/pdf → text extracted as Markdown via unpdf
//   - everything else → error (unsupported content type)
//
// PDF detection also fires on a `%PDF-` magic-byte prefix so misconfigured
// servers (octet-stream, missing content-type) still get extracted instead
// of returning a binary blob.
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

/// Maximum raw response body size (32 MB).  Matches the PDF cap used
/// elsewhere in the codebase (`media::pdf::MAX_PDF_BYTES`) so the body
/// limit isn't the bottleneck for fetching real-world documents
/// (whitepapers, RFC bundles, datasheets).  Extracted text is bounded
/// separately by `MAX_EXTRACTED_PDF_BYTES` for PDFs and by the
/// `max_length` clamp at the end for everything else, so the LLM
/// context exposure stays small even when the download is large.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Per-request timeout — web pages should respond fast; the shared client's
/// 300 s default is for LLM streaming, not page fetches.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum URL length to prevent abuse.
const MAX_URL_LENGTH: usize = 2048;

/// Refuse PDFs that expand to more than this many bytes of extracted text.
/// A malicious 2 MB PDF can in principle decompress to hundreds of MB through
/// stream filters; this caps the post-extraction blow-up well above any
/// realistic article length but far below "fill all RAM".
const MAX_EXTRACTED_PDF_BYTES: usize = 4 * 1024 * 1024;

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
        if !should_pin_for(&verified_url.url) {
            return Ok(self.client.clone());
        }
        crate::http::pinned_client_for_validated_url(verified_url)
            .map_err(|e| DysonError::tool("web_fetch", format!("build pinned client: {e}")))
    }
}

/// Should the fetch go through the pinned-builder client?
///
/// Yes for every URL, so the shared client (which honors HTTP_PROXY /
/// HTTPS_PROXY env vars) cannot route validated traffic through an
/// attacker-controlled proxy. The carve-out for IP-literal hosts that
/// used to live here was an SSRF hardening gap.
fn should_pin_for(_url: &reqwest::Url) -> bool {
    true
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
         stripped of tags, scripts, and styles; PDFs are text-extracted as \
         Markdown; JSON is pretty-printed. Use this instead of curl when you \
         need page content without raw markup — it saves tokens and is easier \
         to work with."
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

        // --- Route on content type ---
        //
        // PDF is checked first (by header and by magic bytes) so misconfigured
        // servers that return `application/octet-stream` or no content-type
        // still get text-extracted instead of dropping into the UTF-8 paths.
        let is_pdf =
            content_type.contains("application/pdf") || body_bytes.starts_with(b"%PDF-");

        let text = if is_pdf {
            let bytes = std::mem::take(&mut body_bytes);
            let extracted = tokio::task::spawn_blocking(move || {
                let doc = unpdf::parse_bytes(&bytes).map_err(|e| e.to_string())?;
                let opts = unpdf::render::RenderOptions::default();
                unpdf::render::to_markdown(&doc, &opts).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| DysonError::tool("web_fetch", format!("PDF task panicked: {e}")))?;

            let text = match extracted {
                Ok(t) => t,
                Err(e) => {
                    return Ok(ToolOutput::error(format!(
                        "failed to extract PDF text from {url}: {e}",
                    )));
                }
            };

            if text.len() > MAX_EXTRACTED_PDF_BYTES {
                return Ok(ToolOutput::error(format!(
                    "PDF at {url} expanded to {:.1} MB of text (limit {} MB) — refusing to process",
                    text.len() as f64 / (1024.0 * 1024.0),
                    MAX_EXTRACTED_PDF_BYTES / (1024 * 1024),
                )));
            }
            if text.trim().is_empty() {
                return Ok(ToolOutput::success(format!(
                    "Content from {url}: (PDF contained no extractable text)",
                )));
            }
            text
        } else {
            let body_str = String::from_utf8_lossy(&body_bytes);
            if content_type.contains("text/html") {
                nanohtml2text::html2text(&body_str)
            } else if content_type.contains("application/json") {
                // Try to pretty-print; fall back to raw.
                match serde_json::from_str::<serde_json::Value>(&body_str) {
                    Ok(val) => serde_json::to_string_pretty(&val)
                        .unwrap_or_else(|_| body_str.into_owned()),
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
                     This tool handles text/html, text/plain, application/json, \
                     and application/pdf.",
                )));
            }
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

    // H5: the IP-literal carve-out used to return the shared client, which
    // honors HTTP_PROXY/HTTPS_PROXY env vars and would let an attacker who
    // controls them re-route validated traffic. Every URL — IP-literal or
    // hostname — must go through the pinned-builder client.
    #[test]
    fn should_pin_for_ip_literal_v4() {
        let url = reqwest::Url::parse("http://198.51.100.7/").unwrap();
        assert!(should_pin_for(&url), "IPv4 literal must use the pinned client");
    }

    #[test]
    fn should_pin_for_ip_literal_v6() {
        let url = reqwest::Url::parse("http://[2001:db8::1]/").unwrap();
        assert!(should_pin_for(&url), "IPv6 literal must use the pinned client");
    }

    #[test]
    fn should_pin_for_hostname() {
        let url = reqwest::Url::parse("https://example.com/").unwrap();
        assert!(should_pin_for(&url), "hostnames must use the pinned client");
    }

    // -----------------------------------------------------------------------
    // PDF support
    // -----------------------------------------------------------------------

    /// Serve `body` once over a localhost listener and return the bound addr.
    /// The HTTP server reads at most one request line, then writes the headers
    /// and body and closes the connection.
    async fn serve_once(content_type: &'static str, body: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut stream, _peer)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: {content_type}\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len(),
                )
                .into_bytes();
                resp.extend_from_slice(&body);
                let _ = stream.write_all(&resp).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn pdf_content_type_extracted_as_text() {
        let pdf_bytes = minimal_pdf_with_text("Hello Dyson PDF");
        let addr = serve_once("application/pdf", pdf_bytes).await;

        let t = WebFetchTool {
            client: crate::http::client().clone(),
            preflight: Some(preflight_to(addr)),
        };
        let ctx = ToolContext::from_cwd().unwrap();
        let url = format!("http://pdf.test:{}/sample.pdf", addr.port());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.run(&json!({ "url": url }), &ctx),
        )
        .await
        .expect("timed out")
        .unwrap();

        assert!(
            !result.is_error,
            "expected success, got error: {}",
            result.content,
        );
        assert!(
            result.content.contains("Hello Dyson PDF"),
            "expected extracted PDF text, got: {}",
            result.content,
        );
    }

    #[tokio::test]
    async fn pdf_magic_bytes_detected_under_octet_stream() {
        let pdf_bytes = minimal_pdf_with_text("Magic bytes win");
        let addr = serve_once("application/octet-stream", pdf_bytes).await;

        let t = WebFetchTool {
            client: crate::http::client().clone(),
            preflight: Some(preflight_to(addr)),
        };
        let ctx = ToolContext::from_cwd().unwrap();
        let url = format!("http://pdf.test:{}/blob", addr.port());

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.run(&json!({ "url": url }), &ctx),
        )
        .await
        .expect("timed out")
        .unwrap();

        assert!(
            !result.is_error,
            "expected success via magic-byte detection, got: {}",
            result.content,
        );
        assert!(
            result.content.contains("Magic bytes win"),
            "expected extracted PDF text, got: {}",
            result.content,
        );
    }

    /// Minimal valid PDF with one line of extractable text.
    /// Identical to the helper in `media/pdf.rs`'s test module; duplicated
    /// rather than shared because both are private test fixtures and a
    /// `pub(crate)` export would leak fixture code into the binary.
    fn minimal_pdf_with_text(text: &str) -> Vec<u8> {
        let page_content = format!("BT /F1 12 Tf 100 700 Td ({text}) Tj ET");
        let stream_len = page_content.len();

        let header = b"%PDF-1.4\n";
        let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2 = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n";
        let obj3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n";
        let obj4 = format!(
            "4 0 obj\n<< /Length {stream_len} >>\nstream\n{page_content}\nendstream\nendobj\n"
        );
        let obj4 = obj4.as_bytes();
        let obj5 = b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n";

        let off1 = header.len();
        let off2 = off1 + obj1.len();
        let off3 = off2 + obj2.len();
        let off4 = off3 + obj3.len();
        let off5 = off4 + obj4.len();

        let mut body = Vec::new();
        body.extend_from_slice(header);
        body.extend_from_slice(obj1);
        body.extend_from_slice(obj2);
        body.extend_from_slice(obj3);
        body.extend_from_slice(obj4);
        body.extend_from_slice(obj5);

        let xref_offset = body.len();
        let xref = format!(
            "xref\n0 6\n\
             0000000000 65535 f \n\
             {off1:010} 00000 n \n\
             {off2:010} 00000 n \n\
             {off3:010} 00000 n \n\
             {off4:010} 00000 n \n\
             {off5:010} 00000 n \n"
        );
        body.extend_from_slice(xref.as_bytes());
        body.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        body
    }
}
