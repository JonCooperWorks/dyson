// ===========================================================================
// WebController — public-facing HTTP agent with restricted tools.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements a hardened HTTP controller that exposes only web_search and
//   web_fetch tools.  Designed for public-facing deployments where the agent
//   should be able to search and browse the web but NOT access the filesystem,
//   run commands, or modify the workspace.
//
// Security model:
//   - Tool restriction: Only web_search + web_fetch (via BuiltinSkill filter)
//   - SSRF protection: PolicySandbox blocks internal/private URLs (always on)
//   - Sandbox always active: Hardcoded dangerous_no_sandbox=false — ignores
//     the parent process's --dangerous-no-sandbox flag
//   - No workspace: No identity, memory, or file tools
//   - No dreams: nudge_interval=0, no workspace → dream system never fires
//   - No MCP/subagent/local skills: Skills list is hardcoded
//   - Per-channel isolation: Each channel gets its own agent + chat history
//   - Concurrency limit: Semaphore prevents resource exhaustion
//
// Per-channel architecture:
//   Like the Telegram controller, each channel gets its own agent instance
//   and persisted conversation history with date-stamped files:
//
//   {history_dir}/
//     channel1/
//       2025-10-08-chat.json    ← today's conversation
//       2025-10-07-chat.json    ← yesterday's
//     channel2/
//       2025-10-08-chat.json
//
// HTTP endpoints:
//   POST /api/chat  — {"channel": "name", "message": "..."}  → SSE stream
//   GET  /health    — 200 OK
// ===========================================================================

mod output;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::agent::{Agent, AgentBuilder};
use crate::chat_history::{ChatHistory, DiskChatHistory};
use crate::config::{ControllerConfig, Settings};
use crate::controller::{Controller, Output};
use crate::error::Result;
use crate::skill::builtin::BuiltinSkill;
use crate::workspace::openclaw::resolve_tilde;

use self::output::{SseEvent, SseOutput};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Controller-specific configuration from the opaque JSON blob.
#[derive(serde::Deserialize)]
struct WebControllerConfig {
    /// Address to bind to (e.g. "0.0.0.0:3000").
    #[serde(default = "default_bind_address")]
    bind_address: String,

    /// Maximum concurrent requests.
    #[serde(default = "default_max_concurrent")]
    max_concurrent: usize,

    /// Directory for per-channel chat history.
    #[serde(default = "default_history_dir")]
    history_dir: String,
}

fn default_bind_address() -> String {
    "0.0.0.0:3000".into()
}
fn default_max_concurrent() -> usize {
    10
}
fn default_history_dir() -> String {
    "~/.dyson/public".into()
}

// ---------------------------------------------------------------------------
// Per-channel state
// ---------------------------------------------------------------------------

/// A single channel's agent and chat history.
struct ChannelEntry {
    agent: Mutex<Agent>,
    chat_store: DiskChatHistory,
}

// ---------------------------------------------------------------------------
// WebController
// ---------------------------------------------------------------------------

/// Public-facing HTTP controller with restricted web-only tools.
pub struct WebController {
    config: WebControllerConfig,
}

impl WebController {
    /// Create from a ControllerConfig by parsing the opaque JSON blob.
    pub fn from_config(config: &ControllerConfig) -> Option<Self> {
        let web_config: WebControllerConfig =
            serde_json::from_value(config.config.clone()).ok()?;
        Some(Self {
            config: web_config,
        })
    }

    /// Build a restricted agent with only web_search + web_fetch tools.
    ///
    /// SECURITY: Always creates a PolicySandbox regardless of the parent
    /// process's --dangerous-no-sandbox flag.  The SSRF sandbox MUST be
    /// active for a public-facing agent.
    fn build_public_agent(settings: &Settings) -> Result<Agent> {
        let skills: Vec<Box<dyn crate::skill::Skill>> = vec![Box::new(
            BuiltinSkill::new_filtered(
                settings.web_search.as_ref(),
                &["web_search".into(), "web_fetch".into()],
            ),
        )];

        let client = crate::llm::create_client(&settings.agent, None, false);
        // SECURITY: Always false — public agent sandbox is never disabled.
        let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, false);

        AgentBuilder::new(client, sandbox)
            .skills(skills)
            .settings(&settings.agent)
            .build()
    }
}

// ---------------------------------------------------------------------------
// Controller trait implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl Controller for WebController {
    fn name(&self) -> &str {
        "web"
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(
            "You are a web research assistant. You can search the web and fetch web pages \
             to answer questions. You do NOT have access to the filesystem, shell commands, \
             or any workspace tools. Be concise and cite your sources.",
        )
    }

    async fn run(&self, settings: &Settings) -> Result<()> {
        let listener = TcpListener::bind(&self.config.bind_address).await?;
        let addr = listener.local_addr()?;
        tracing::info!(address = %addr, "web controller listening");

        let history_dir = resolve_tilde(&self.config.history_dir);

        // Shared state across all connections.
        let channels: Arc<RwLock<HashMap<String, Arc<ChannelEntry>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrent));
        let settings = Arc::new(settings.clone());
        let history_dir = Arc::new(history_dir);

        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!(error = %e, "web controller accept error");
                    continue;
                }
            };

            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!("web controller at connection limit, dropping connection");
                    continue;
                }
            };

            let channels = Arc::clone(&channels);
            let settings = Arc::clone(&settings);
            let history_dir = Arc::clone(&history_dir);

            tokio::spawn(async move {
                let _permit = permit;
                let io = TokioIo::new(stream);
                let channels = Arc::clone(&channels);
                let settings = Arc::clone(&settings);
                let history_dir = Arc::clone(&history_dir);

                let service = service_fn(move |req| {
                    let channels = Arc::clone(&channels);
                    let settings = Arc::clone(&settings);
                    let history_dir = Arc::clone(&history_dir);
                    async move {
                        let resp = handle_request(req, &channels, &settings, &history_dir).await;
                        Ok::<_, Infallible>(resp)
                    }
                });

                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    tracing::debug!(error = %e, "web controller HTTP connection error");
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP request routing
// ---------------------------------------------------------------------------

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    channels: &RwLock<HashMap<String, Arc<ChannelEntry>>>,
    settings: &Settings,
    history_dir: &std::path::Path,
) -> Response<Full<Bytes>> {
    // CORS headers for all responses.
    let cors_headers = |mut resp: Response<Full<Bytes>>| {
        let h = resp.headers_mut();
        h.insert("access-control-allow-origin", "*".parse().unwrap());
        h.insert(
            "access-control-allow-methods",
            "POST, GET, OPTIONS".parse().unwrap(),
        );
        h.insert(
            "access-control-allow-headers",
            "content-type".parse().unwrap(),
        );
        resp
    };

    match (req.method(), req.uri().path()) {
        // Health check.
        (&hyper::Method::GET, "/health") => {
            cors_headers(json_response(StatusCode::OK, &serde_json::json!({"status": "ok"})))
        }

        // CORS preflight.
        (&hyper::Method::OPTIONS, _) => {
            cors_headers(Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Full::new(Bytes::new()))
                .unwrap())
        }

        // Chat endpoint.
        (&hyper::Method::POST, "/api/chat") => {
            cors_headers(handle_chat(req, channels, settings, history_dir).await)
        }

        // 404.
        _ => cors_headers(json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({"error": "not found"}),
        )),
    }
}

// ---------------------------------------------------------------------------
// Chat handler
// ---------------------------------------------------------------------------

/// Request body for POST /api/chat.
#[derive(serde::Deserialize)]
struct ChatRequest {
    channel: String,
    message: String,
}

async fn handle_chat(
    req: Request<hyper::body::Incoming>,
    channels: &RwLock<HashMap<String, Arc<ChannelEntry>>>,
    settings: &Settings,
    history_dir: &std::path::Path,
) -> Response<Full<Bytes>> {
    // Parse request body.
    let body = match http_body_util::BodyExt::collect(req.into_body()).await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("failed to read body: {e}")}),
            );
        }
    };

    let chat_req: ChatRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({"error": format!("invalid JSON: {e}")}),
            );
        }
    };

    // Validate channel name: alphanumeric, hyphens, underscores only.
    if chat_req.channel.is_empty()
        || !chat_req
            .channel
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "channel must be non-empty and contain only alphanumeric characters, hyphens, and underscores"}),
        );
    }

    if chat_req.message.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "message must not be empty"}),
        );
    }

    // Get or create channel entry.
    let entry = match get_or_create_channel(
        &chat_req.channel,
        channels,
        settings,
        history_dir,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, channel = chat_req.channel, "failed to create channel");
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": "failed to initialize agent"}),
            );
        }
    };

    // Run the agent and stream SSE output.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SseEvent>(64);

    // Spawn agent execution in a background task.
    let message = chat_req.message;
    let channel_name = chat_req.channel;
    tokio::spawn(async move {
        let mut output = SseOutput::new(tx.clone());

        let mut agent = entry.agent.lock().await;
        match agent.run(&message, &mut output).await {
            Ok(_) => {
                // Persist chat history.
                let chat_id = today_chat_id();
                if let Err(e) = entry.chat_store.save(&chat_id, agent.messages()) {
                    tracing::error!(
                        error = %e,
                        channel = channel_name,
                        "failed to save chat history"
                    );
                }
            }
            Err(e) => {
                tracing::error!(error = %e, channel = channel_name, "agent run failed");
                let _ = output.error(&e);
            }
        }

        // Send done event.
        let _ = tx
            .send(SseEvent {
                event: "done",
                data: serde_json::json!({}),
            })
            .await;
    });

    // Collect SSE events into a response body.
    // For true streaming we'd need a streaming body, but hyper's Full<Bytes>
    // is simpler and sufficient for moderate response sizes.  The agent
    // completes, then we send all events at once.
    let mut sse_bytes: Vec<u8> = Vec::new();
    while let Some(event) = rx.recv().await {
        sse_bytes.extend_from_slice(&event.to_sse_bytes());
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Full::new(Bytes::from(sse_bytes)))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Channel management
// ---------------------------------------------------------------------------

/// Get or create a channel entry (agent + chat store).
///
/// Fast path: channel exists in the map.
/// Slow path: build a new agent, create chat store, load today's history.
async fn get_or_create_channel(
    channel: &str,
    channels: &RwLock<HashMap<String, Arc<ChannelEntry>>>,
    settings: &Settings,
    history_dir: &std::path::Path,
) -> Result<Arc<ChannelEntry>> {
    // Fast path.
    {
        let map = channels.read().await;
        if let Some(entry) = map.get(channel) {
            return Ok(Arc::clone(entry));
        }
    }

    // Slow path — build agent and chat store.
    let channel_dir = history_dir.join(channel);
    let chat_store = DiskChatHistory::new(channel_dir)?;

    let mut agent = WebController::build_public_agent(settings)?;

    // Load today's history.
    let chat_id = today_chat_id();
    let messages = chat_store.load(&chat_id)?;
    if !messages.is_empty() {
        tracing::info!(
            channel = channel,
            messages = messages.len(),
            "restored chat history"
        );
        agent.set_messages(messages);
    }

    let entry = Arc::new(ChannelEntry {
        agent: Mutex::new(agent),
        chat_store,
    });

    let mut map = channels.write().await;
    // Double-check: another task may have created it while we were building.
    if let Some(existing) = map.get(channel) {
        return Ok(Arc::clone(existing));
    }
    map.insert(channel.to_string(), Arc::clone(&entry));

    tracing::info!(channel = channel, "new channel initialized");
    Ok(entry)
}

/// Generate today's chat ID (e.g. "2025-10-08-chat").
fn today_chat_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d) = crate::util::unix_to_ymd(secs);
    format!("{y:04}-{m:02}-{d:02}-chat")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, body: &serde_json::Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_string(body).unwrap_or_else(|_| "{}".into()),
        )))
        .unwrap()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_chat_id_format() {
        let id = today_chat_id();
        // Should be like "2026-04-08-chat"
        assert!(id.ends_with("-chat"), "got: {id}");
        assert_eq!(id.len(), "2026-04-08-chat".len());
        // Year should be 4 digits
        assert!(id[0..4].parse::<u32>().is_ok());
    }

    #[test]
    fn channel_name_validation() {
        // Valid names
        for name in &["general", "my-channel", "test_123", "ABC"] {
            assert!(
                name.chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
                "'{name}' should be valid"
            );
        }
        // Invalid names (path traversal, special chars)
        for name in &["../etc", "chan nel", "foo/bar", "a;b", ""] {
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
            assert!(!valid, "'{name}' should be invalid");
        }
    }

    #[test]
    fn sse_event_format() {
        let event = SseEvent {
            event: "text_delta",
            data: serde_json::json!({"text": "hello"}),
        };
        let bytes = event.to_sse_bytes();
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("event: text_delta\n"));
        assert!(s.contains("data: "));
        assert!(s.contains("\"text\":\"hello\""));
        assert!(s.ends_with("\n\n"));
    }
}
