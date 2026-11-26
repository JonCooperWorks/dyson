//! Streamable HTTP MCP transport: one POST per JSON-RPC message.

use super::*;

// ---------------------------------------------------------------------------
// HttpTransport — Streamable HTTP MCP
// ---------------------------------------------------------------------------

/// Communicates with an MCP server over HTTP POST requests.
///
/// Each JSON-RPC message is sent as the body of a POST request.  The
/// response body contains the JSON-RPC response.  Custom headers (like
/// API keys) are sent with every request.
///
/// Used by MCP servers like Context7 that expose an HTTP endpoint
/// instead of running as a local process.
///
/// ```json
/// // In dyson.json:
/// "mcp_servers": {
///   "context7": {
///     "url": "https://mcp.context7.com/mcp",
///     "headers": { "CONTEXT7_API_KEY": "your-key" }
///   }
/// }
/// ```
pub struct HttpTransport {
    /// HTTP client (connection pooling, TLS).
    client: reqwest::Client,

    /// The MCP server URL to POST to.
    url: String,

    /// Authentication handler for outgoing requests.
    /// Applies headers (API keys, auth tokens, etc.) via the Auth trait.
    auth: Box<dyn crate::auth::Auth>,

    /// Request ID counter.
    next_id: AtomicU64,

    /// Session ID returned by the server (if any).
    /// Some MCP HTTP servers return a session ID in the response headers
    /// that must be sent back in subsequent requests.
    session_id: Mutex<Option<String>>,

    /// Handler for server-originated requests/notifications that arrive
    /// interleaved in an SSE response body.  Defaults to
    /// [`UnsupportedInboundHandler`] until a skill installs its router.
    inbound: std::sync::Mutex<Arc<dyn InboundHandler>>,
}

impl HttpTransport {
    /// Create a new HTTP transport.
    ///
    /// `url` is the MCP server endpoint.
    /// `auth` applies authentication headers to every request.
    pub fn new(url: &str, auth: Box<dyn crate::auth::Auth>) -> Self {
        Self {
            // No redirects: reqwest only strips `Authorization` on a
            // cross-origin hop, so an `ApiKeyAuth` / `StaticHeadersAuth`
            // header would be replayed to wherever the server points us.
            client: crate::http::no_redirect_client().clone(),
            url: url.to_string(),
            auth,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            inbound: std::sync::Mutex::new(Arc::new(UnsupportedInboundHandler)),
        }
    }

    /// POST a fire-and-forget JSON-RPC message (a notification, or our
    /// response to a server-originated request) to the MCP endpoint.
    /// Streamable HTTP servers accept these as ordinary POSTs and reply
    /// 202 Accepted with no body, so we don't read the response.
    async fn post_message(&self, json: &str) {
        let Ok(mut req) = self.build_request(json).await else {
            return;
        };
        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }
        let _ = req.send().await;
    }

    /// Dispatch one server-originated frame seen in an SSE response body:
    /// a request gets routed to the inbound handler and its response
    /// POSTed back; a notification is routed and dropped.  Returns the
    /// value verbatim for the caller to treat as the response to our
    /// outbound request when it is neither (i.e. a `Response` frame).
    async fn dispatch_inbound_frame(&self, value: Value) -> Option<Value> {
        match classify_inbound(&value) {
            Inbound::Response => Some(value),
            Inbound::Request { id, method, params } => {
                let handler = Arc::clone(&self.inbound.lock().unwrap());
                let result = handler.handle_request(&method, params).await;
                self.post_message(&build_response_line(&id, result)).await;
                None
            }
            Inbound::Notification { method, params } => {
                let handler = Arc::clone(&self.inbound.lock().unwrap());
                handler.handle_notification(&method, params).await;
                None
            }
        }
    }

    /// Build a request with common headers.
    async fn build_request(
        &self,
        body: &str,
    ) -> dyson_core::error::Result<reqwest::RequestBuilder> {
        let req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_string());

        // Apply auth headers (API keys, bearer tokens, etc.).
        self.auth.apply_to_request(req).await
    }

    /// Send an HTTP request and capture the session ID from the response.
    ///
    /// Shared by the normal path and the 401 retry path to avoid duplication.
    async fn send_http(&self, json: &str) -> dyson_core::error::Result<reqwest::Response> {
        let mut req = self.build_request(json).await?;

        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }

        let response = req.send().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("HTTP request failed: {e}"),
        })?;

        if let Some(sid) = response.headers().get("mcp-session-id")
            && let Ok(sid_str) = sid.to_str()
        {
            let mut session = self.session_id.lock().await;
            *session = Some(sid_str.to_string());
        }

        Ok(response)
    }

    /// Parse an HTTP response into a JSON-RPC result.
    ///
    /// Streamable HTTP MCP servers may return either `application/json`
    /// (single response in the body) or `text/event-stream` (one or
    /// more `event: message\ndata: <json>\n\n` frames).  Context7,
    /// GitHub MCP, Linear, and most reference servers prefer SSE — the
    /// initial-only-JSON path was the previous behavior and silently
    /// dropped any SSE-shaped response with "expected value at line 1".
    ///
    /// For SSE bodies the parse is *incremental*: each complete `data:`
    /// frame is dispatched as soon as its terminating blank line
    /// arrives, so a server-originated `elicitation/create` reaches
    /// the inbound handler — and its answer goes out on a separate
    /// POST — before the upstream tools/call finishes.  That's the
    /// difference between "elicitation works" and "elicitation
    /// deadlocks waiting on the body to close."
    ///
    /// Shared by the normal path and the 401 retry path.
    async fn parse_rpc_response(
        &self,
        response: reqwest::Response,
    ) -> dyson_core::error::Result<serde_json::Value> {
        if !response.status().is_success() {
            let status = response.status();
            let body = match read_response_text_capped(response, &self.url).await {
                Ok(body) => body,
                Err(e) if e.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE) => return Err(e),
                Err(_) => "failed to read body".into(),
            };
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("HTTP {status}: {body}"),
            });
        }

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/event-stream"))
            });

        if !is_sse {
            // Non-SSE: the whole body is the single JSON-RPC response,
            // safe to buffer.
            let body = read_response_text_capped(response, &self.url).await?;
            return self.finish_rpc_value(parse_rpc_value(&body, &self.url)?);
        }

        // SSE: stream the body, dispatching each complete frame as it
        // arrives so server-originated requests can be answered while
        // the body is still open.
        self.stream_sse_response(response).await
    }

    /// Incrementally parse an SSE response, dispatching each frame
    /// (response, server-originated request, notification) the moment
    /// its terminating blank line is observed.
    async fn stream_sse_response(
        &self,
        mut response: reqwest::Response,
    ) -> dyson_core::error::Result<serde_json::Value> {
        let mut parser = SseStreamParser::new();
        let mut total_bytes = 0usize;
        let mut answer: Option<Value> = None;
        while let Some(chunk) = response.chunk().await.map_err(|e| DysonError::Mcp {
            server: self.url.clone(),
            message: format!("failed to read SSE chunk: {e}"),
        })? {
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > MAX_MCP_RESULT_BYTES {
                return Err(mcp_result_too_large(&self.url));
            }
            let new_frames = parser.feed(&chunk);
            for frame in new_frames {
                let value = parse_rpc_value(&frame, &self.url)?;
                if let Some(resp) = self.dispatch_inbound_frame(value).await {
                    answer = Some(resp);
                }
            }
        }
        // Flush any trailing frame the server forgot to terminate with
        // a blank line.
        if let Some(frame) = parser.finish() {
            let value = parse_rpc_value(&frame, &self.url)?;
            if let Some(resp) = self.dispatch_inbound_frame(value).await {
                answer = Some(resp);
            }
        }

        match answer {
            Some(v) => self.finish_rpc_value(v),
            None => Err(DysonError::Mcp {
                server: self.url.clone(),
                message: "SSE response carried no response to our request".into(),
            }),
        }
    }

    /// Turn a parsed JSON-RPC response value into the `result`, mapping a
    /// JSON-RPC `error` object onto a `DysonError`.
    fn finish_rpc_value(&self, value: Value) -> dyson_core::error::Result<serde_json::Value> {
        let rpc_response: JsonRpcResponse =
            serde_json::from_value(value).map_err(|e| DysonError::Mcp {
                server: self.url.clone(),
                message: format!("failed to parse response: {e}"),
            })?;

        if let Some(err) = rpc_response.error {
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("RPC error {}: {}", err.code, err.message),
            });
        }

        rpc_response.result.ok_or_else(|| DysonError::Mcp {
            server: self.url.clone(),
            message: "response has neither result nor error".into(),
        })
    }
}

/// Parse one JSON-RPC frame (a single `data:` payload or a non-SSE body)
/// into a `Value`, attributing parse failures to `server`.
fn parse_rpc_value(raw: &str, server: &str) -> dyson_core::error::Result<Value> {
    serde_json::from_str(raw).map_err(|e| DysonError::Mcp {
        server: server.to_string(),
        message: format!("failed to parse response: {e}"),
    })
}

/// Extract the last `data:` payload from a Server-Sent Events body.
///
/// Streamable HTTP MCP servers wrap the JSON-RPC response in an
/// `event: message\ndata: {...}\n\n` envelope (RFC-style SSE).  For
/// the request/response shape MCP uses for initialize / tools/list /
/// tools/call, exactly one frame is emitted — but defensively we
/// take the LAST `data:` line so future server behavior (a status
/// frame followed by the response, etc.) doesn't trip the parser.
#[cfg(test)]
pub(super) fn extract_last_sse_data(body: &str) -> Option<String> {
    extract_all_sse_frames(body).pop()
}

/// Extract every `data:` payload from a Server-Sent Events body, in
/// order.  Retained as a test helper for fixture-based tests — the
/// production path uses [`SseStreamParser`] so dispatch happens as
/// each frame arrives, not after the body terminates.
#[cfg(test)]
pub(super) fn extract_all_sse_frames(body: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let chunk = rest.strip_prefix(' ').unwrap_or(rest);
            current = Some(match current.take() {
                Some(prev) => format!("{prev}\n{chunk}"),
                None => chunk.to_string(),
            });
        } else if line.is_empty()
            && let Some(c) = current.take()
        {
            frames.push(c);
        }
    }
    if let Some(c) = current {
        frames.push(c);
    }
    frames
}

/// Incremental SSE parser.  Accumulates bytes across reqwest chunks and
/// emits each complete `data:` event as soon as the terminating blank
/// line arrives, so the caller can dispatch frames before the rest of
/// the body has been read.
///
/// Matches [`extract_all_sse_frames`]'s semantics for buffered bodies:
/// every `data:` payload becomes one frame; multiple `data:` lines in
/// one event are joined with `\n`; the trailing blank line is optional
/// (we surface the final frame via [`SseStreamParser::finish`]).
struct SseStreamParser {
    buf: Vec<u8>,
    current: Option<String>,
}

impl SseStreamParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(newline) = self.buf.iter().position(|b| *b == b'\n') {
            let raw_line: Vec<u8> = self.buf.drain(..=newline).collect();
            // Drop the trailing `\n` and any preceding `\r`.
            let mut len = raw_line.len() - 1;
            if len > 0 && raw_line[len - 1] == b'\r' {
                len -= 1;
            }
            let line = match std::str::from_utf8(&raw_line[..len]) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    // Non-UTF8 line: skip it.  This matches the
                    // buffered parser's behaviour of dropping garbage
                    // lines silently.
                    continue;
                }
            };
            if let Some(rest) = line.strip_prefix("data:") {
                let chunk = rest.strip_prefix(' ').unwrap_or(rest);
                self.current = Some(match self.current.take() {
                    Some(prev) => format!("{prev}\n{chunk}"),
                    None => chunk.to_string(),
                });
            } else if line.is_empty()
                && let Some(frame) = self.current.take()
            {
                frames.push(frame);
            }
            // Other SSE lines (event:, id:, retry:, comments starting
            // with `:`) are intentionally ignored — the wire format
            // we accept is exactly the one Streamable HTTP MCP emits:
            // `event: message\ndata: <json>\n\n`.
        }
        frames
    }

    /// Flush a trailing frame the server omitted to terminate with a
    /// blank line.  Mirrors the lenient behaviour of
    /// [`extract_all_sse_frames`] / [`extract_last_sse_data`].
    fn finish(mut self) -> Option<String> {
        // Drain any unterminated line in `buf` as a tail event.
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            if let Ok(line) = std::str::from_utf8(&raw) {
                let trimmed = line.strip_suffix('\r').unwrap_or(line);
                if let Some(rest) = trimmed.strip_prefix("data:") {
                    let chunk = rest.strip_prefix(' ').unwrap_or(rest);
                    self.current = Some(match self.current.take() {
                        Some(prev) => format!("{prev}\n{chunk}"),
                        None => chunk.to_string(),
                    });
                }
            }
        }
        self.current.take()
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = JsonRpcRequest::new(id, method, params);
        let json = serde_json::to_string(&request)?;
        enforce_mcp_request_cap(&self.url, &json)?;

        let response = self.send_http(&json).await?;

        // 401 Unauthorized — refresh credentials and retry once.
        //
        // Handles OAuth token rejection (clock skew, server-side revocation)
        // when the token hasn't expired locally.  on_unauthorized() gives
        // OAuth a chance to force-refresh before we retry.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::debug!(server = %self.url, "received 401 — attempting credential refresh");

            if self.auth.on_unauthorized().await.is_ok() {
                let retry_response = self.send_http(&json).await?;
                return self.parse_rpc_response(retry_response).await;
            }

            let body = match read_response_text_capped(response, &self.url).await {
                Ok(body) => body,
                Err(e) if e.to_string().contains(MCP_RESULT_TOO_LARGE_MESSAGE) => return Err(e),
                Err(_) => "failed to read body".into(),
            };
            return Err(DysonError::Mcp {
                server: self.url.clone(),
                message: format!("HTTP 401 Unauthorized: {body}"),
            });
        }

        self.parse_rpc_response(response).await
    }

    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let json = serde_json::to_string(&notification)?;
        enforce_mcp_request_cap(&self.url, &json)?;

        let mut req = self.build_request(&json).await?;

        {
            let session = self.session_id.lock().await;
            if let Some(ref sid) = *session {
                req = req.header("Mcp-Session-Id", sid.as_str());
            }
        }

        // Fire and forget — we don't check the response for notifications.
        let _ = req.send().await;

        Ok(())
    }

    fn set_inbound_handler(&self, handler: Arc<dyn InboundHandler>) {
        *self.inbound.lock().unwrap() = handler;
    }
}

// ===========================================================================
// Tests
// ===========================================================================
