// ===========================================================================
// SwarmConnection — SSE inbound + POST outbound to the swarm hub.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Manages the connection between a Dyson node and the swarm hub.
//   Two protocols, one struct:
//
//   Inbound (hub → node):  SSE stream on GET {url}/swarm/events
//   Outbound (node → hub): POST requests to /swarm/register, /heartbeat,
//                           /result, /blob
//
// Why SSE instead of WebSocket?
//   SSE is HTTP.  It reuses the same reqwest client, the same TLS config,
//   the same proxy settings.  No new dependency.
// ===========================================================================

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;

use crate::error::{DysonError, Result};
use crate::swarm::types::{NodeManifest, NodeStatus, SwarmResult, TaskCheckpoint};

// ---------------------------------------------------------------------------
// SSE event types
// ---------------------------------------------------------------------------

/// A parsed SSE event from the hub.
#[derive(Debug)]
pub enum SwarmEvent {
    /// Hub acknowledged registration.
    Registered { node_id: String },
    /// Hub is sending a signed task (raw wire bytes: version || signature || JSON).
    Task(Vec<u8>),
    /// Hub acknowledged a heartbeat.
    HeartbeatAck,
    /// Hub requests cancellation of the named task.
    CancelTask { task_id: String },
    /// Hub is requesting graceful shutdown.
    Shutdown,
}

// ---------------------------------------------------------------------------
// SwarmConnection
// ---------------------------------------------------------------------------

/// Connection to a swarm hub.
///
/// Clone is cheap (reqwest::Client is Arc-based internally).
#[derive(Clone)]
pub struct SwarmConnection {
    base_url: String,
    /// Client for POST requests (has request timeout).
    client: reqwest::Client,
    /// Client for SSE stream (no request timeout — connection stays open).
    sse_client: reqwest::Client,
    auth_token: Option<String>,
}

impl SwarmConnection {
    /// Create a new connection to the hub.
    pub fn new(base_url: &str) -> Self {
        crate::http::ensure_crypto_provider();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            sse_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build SSE client"),
            auth_token: None,
        }
    }

    /// Apply bearer auth to a request builder if a token is set.
    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.auth_token {
            Some(ref token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// Check response status; return a `Swarm` error with body on failure.
    async fn check(resp: reqwest::Response, context: &str) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(DysonError::Swarm(format!("{context}: {status} — {body}")))
    }

    // -----------------------------------------------------------------------
    // Outbound: POST requests
    // -----------------------------------------------------------------------

    /// Register this node with the hub.
    pub async fn register(&mut self, manifest: &NodeManifest) -> Result<RegisterResponse> {
        let url = format!("{}/swarm/register", self.base_url);
        let resp = self.client.post(&url).json(manifest).send().await?;
        let resp = Self::check(resp, "registration failed").await?;

        let reg: RegisterResponse = resp.json().await.map_err(|e| {
            DysonError::Swarm(format!("failed to parse registration response: {e}"))
        })?;

        self.auth_token = Some(reg.token.clone());
        Ok(reg)
    }

    /// Send a heartbeat to the hub.
    pub async fn heartbeat(&self, status: &NodeStatus) -> Result<()> {
        let url = format!("{}/swarm/heartbeat", self.base_url);
        let req = self.authed(self.client.post(&url).json(status));
        let resp = req.send().await?;
        Self::check(resp, "heartbeat failed").await?;
        Ok(())
    }

    /// Send a task result back to the hub.
    pub async fn send_result(&self, result: &SwarmResult) -> Result<()> {
        let url = format!("{}/swarm/result", self.base_url);
        let req = self.authed(self.client.post(&url).json(result));
        let resp = req.send().await?;
        Self::check(resp, "result submission failed").await?;
        Ok(())
    }

    /// POST a progress checkpoint for an in-flight task.
    ///
    /// Non-fatal for the task if it fails — the checkpoint is best-effort
    /// metadata.  Callers should log send failures and continue executing.
    pub async fn send_checkpoint(&self, checkpoint: &TaskCheckpoint) -> Result<()> {
        let url = format!("{}/swarm/checkpoint", self.base_url);
        let req = self.authed(self.client.post(&url).json(checkpoint));
        let resp = req.send().await?;
        Self::check(resp, "checkpoint submission failed").await?;
        Ok(())
    }

    /// Fetch a blob by SHA-256 hash from the hub.
    pub async fn fetch_blob(&self, sha256: &str) -> Result<Vec<u8>> {
        let url = format!("{}/swarm/blob/{sha256}", self.base_url);
        let req = self.authed(self.client.get(&url));
        let resp = req.send().await?;
        let resp = Self::check(resp, &format!("blob fetch failed for {sha256}")).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Upload a blob to the hub (for large result payloads).
    pub async fn upload_blob(&self, sha256: &str, data: &[u8]) -> Result<()> {
        let url = format!("{}/swarm/blob/{sha256}", self.base_url);
        let req = self.authed(self.client.put(&url).body(data.to_vec()));
        let resp = req.send().await?;
        Self::check(resp, &format!("blob upload failed for {sha256}")).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Inbound: SSE event stream
    // -----------------------------------------------------------------------

    /// Open the SSE event stream from the hub.
    ///
    /// Returns a receiver that yields `SwarmEvent`s.  The connection
    /// stays open until the hub closes it or the receiver is dropped.
    pub async fn open_event_stream(
        &self,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<SwarmEvent>>> {
        let url = format!("{}/swarm/events", self.base_url);
        let req = self.authed(self.sse_client.get(&url).header("Accept", "text/event-stream"));
        let resp = req.send().await?;

        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::Swarm(format!(
                "SSE connection failed: {status} — {body}"
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(event) = extract_sse_event(&mut buffer) {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(DysonError::Swarm(format!("SSE stream error: {e}"))))
                            .await;
                        return;
                    }
                }
            }

            let _ = tx
                .send(Err(DysonError::Swarm("SSE stream closed by hub".into())))
                .await;
        });

        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// Registration response
// ---------------------------------------------------------------------------

/// Response from the hub after successful registration.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub token: String,
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

/// Extract one complete SSE event from the buffer, if available.
fn extract_sse_event(buffer: &mut String) -> Option<SwarmEvent> {
    let delimiter = buffer.find("\n\n")?;

    let event_text = buffer[..delimiter].to_string();
    buffer.drain(..delimiter + 2);

    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_text.lines() {
        if let Some(val) = line.strip_prefix("event:") {
            event_type = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(val.trim());
        }
    }

    parse_sse_event(&event_type, &data)
}

fn parse_sse_event(event_type: &str, data: &str) -> Option<SwarmEvent> {
    match event_type {
        "registered" => {
            let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
            let node_id = parsed["node_id"].as_str()?.to_string();
            Some(SwarmEvent::Registered { node_id })
        }
        "task" => {
            let wire_bytes = STANDARD.decode(data.trim()).ok()?;
            Some(SwarmEvent::Task(wire_bytes))
        }
        "heartbeat_ack" => Some(SwarmEvent::HeartbeatAck),
        "cancel_task" => {
            let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
            let task_id = parsed["task_id"].as_str()?.to_string();
            Some(SwarmEvent::CancelTask { task_id })
        }
        "shutdown" => Some(SwarmEvent::Shutdown),
        _ => {
            tracing::debug!(event_type, "unknown SSE event type");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_registered() {
        let mut buf = "event: registered\ndata: {\"node_id\": \"abc-123\"}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        match event {
            SwarmEvent::Registered { node_id } => assert_eq!(node_id, "abc-123"),
            _ => panic!("expected Registered"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_sse_task() {
        let wire_data = vec![0x01u8; 70];
        let b64 = STANDARD.encode(&wire_data);
        let mut buf = format!("event: task\ndata: {b64}\n\n");
        let event = extract_sse_event(&mut buf).unwrap();
        match event {
            SwarmEvent::Task(bytes) => assert_eq!(bytes, wire_data),
            _ => panic!("expected Task"),
        }
    }

    #[test]
    fn parse_sse_heartbeat_ack() {
        let mut buf = "event: heartbeat_ack\ndata: {}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        assert!(matches!(event, SwarmEvent::HeartbeatAck));
    }

    #[test]
    fn parse_sse_cancel_task() {
        let mut buf = "event: cancel_task\ndata: {\"task_id\":\"abc-123\"}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        match event {
            SwarmEvent::CancelTask { task_id } => assert_eq!(task_id, "abc-123"),
            _ => panic!("expected CancelTask"),
        }
    }

    #[test]
    fn parse_sse_shutdown() {
        let mut buf = "event: shutdown\ndata: {}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        assert!(matches!(event, SwarmEvent::Shutdown));
    }

    #[test]
    fn parse_sse_unknown_event_ignored() {
        let mut buf = "event: unknown_type\ndata: whatever\n\n".to_string();
        let event = extract_sse_event(&mut buf);
        assert!(event.is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn parse_sse_incomplete_event() {
        let mut buf = "event: task\ndata: AAAA".to_string();
        let event = extract_sse_event(&mut buf);
        assert!(event.is_none());
        assert_eq!(buf, "event: task\ndata: AAAA");
    }

    #[test]
    fn parse_sse_multiple_events() {
        let mut buf =
            "event: heartbeat_ack\ndata: {}\n\nevent: shutdown\ndata: {}\n\n".to_string();

        let e1 = extract_sse_event(&mut buf).unwrap();
        assert!(matches!(e1, SwarmEvent::HeartbeatAck));

        let e2 = extract_sse_event(&mut buf).unwrap();
        assert!(matches!(e2, SwarmEvent::Shutdown));

        assert!(buf.is_empty());
    }

    #[test]
    fn parse_sse_multiline_data() {
        let mut buf = "event: registered\ndata: {\"node_id\":\ndata:  \"abc\"}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        match event {
            SwarmEvent::Registered { node_id } => assert_eq!(node_id, "abc"),
            _ => panic!("expected Registered"),
        }
    }
}
