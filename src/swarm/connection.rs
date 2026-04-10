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
// SSE (Server-Sent Events):
//   A simple HTTP-based push protocol.  The client sends a GET, the server
//   holds the connection open and pushes newline-delimited events:
//
//     event: task
//     data: base64-of-signed-wire-bytes
//
//     event: heartbeat_ack
//     data: {}
//
//   Each event has a type (`event:` line) and payload (`data:` line).
//   Empty lines delimit events.  That's it — no framing, no binary, no
//   negotiation.  reqwest's streaming response handles the chunked
//   transfer encoding transparently.
//
// Why SSE instead of WebSocket?
//   SSE is HTTP.  It reuses the same reqwest client, the same TLS config,
//   the same proxy settings.  No new dependency.  WebSocket would add
//   tokio-tungstenite and a second protocol stack for no real benefit —
//   the outbound direction is already POST requests.
// ===========================================================================

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use reqwest::StatusCode;

use crate::error::{DysonError, Result};
use crate::swarm::types::{NodeManifest, NodeStatus, SwarmResult};

// ---------------------------------------------------------------------------
// SSE event types
// ---------------------------------------------------------------------------

/// A parsed SSE event from the hub.
#[derive(Debug)]
pub enum SwarmEvent {
    /// Hub acknowledged registration.
    Registered { node_id: String },
    /// Hub is sending a signed task.  The bytes are the raw signed wire
    /// format (version || signature || JSON payload).
    Task(Vec<u8>),
    /// Hub acknowledged a heartbeat.
    HeartbeatAck,
    /// Hub is requesting graceful shutdown.
    Shutdown,
}

// ---------------------------------------------------------------------------
// SwarmConnection
// ---------------------------------------------------------------------------

/// Connection to a swarm hub.
///
/// Manages SSE for inbound events and POST for outbound messages.
/// Clone is cheap (reqwest::Client is Arc-based internally).
#[derive(Clone)]
pub struct SwarmConnection {
    /// Base URL of the hub (e.g. "https://hub.example.com").
    base_url: String,
    /// HTTP client (shared process-wide singleton).
    client: reqwest::Client,
    /// Node token for authentication (received after registration,
    /// or derived from public key).  Set after register() succeeds.
    auth_token: Option<String>,
}

impl SwarmConnection {
    /// Create a new connection to the hub.
    pub fn new(base_url: &str) -> Self {
        // Ensure TLS crypto provider is installed (idempotent).
        crate::http::ensure_crypto_provider();

        // Build a separate client for SSE with no request timeout
        // (the SSE stream stays open indefinitely).
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                // No overall timeout — SSE streams are long-lived.
                .build()
                .expect("failed to build SSE HTTP client"),
            auth_token: None,
        }
    }

    /// Set the auth token (received from hub after registration).
    pub fn set_auth_token(&mut self, token: String) {
        self.auth_token = Some(token);
    }

    // -----------------------------------------------------------------------
    // Outbound: POST requests
    // -----------------------------------------------------------------------

    /// Register this node with the hub.
    ///
    /// Returns the node_id and auth token assigned by the hub.
    pub async fn register(
        &mut self,
        manifest: &NodeManifest,
    ) -> Result<RegisterResponse> {
        let url = format!("{}/swarm/register", self.base_url);
        let resp = self.client.post(&url).json(manifest).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::Swarm(format!(
                "registration failed: {status} — {body}"
            )));
        }

        let reg: RegisterResponse = resp.json().await.map_err(|e| {
            DysonError::Swarm(format!("failed to parse registration response: {e}"))
        })?;

        self.auth_token = Some(reg.token.clone());
        Ok(reg)
    }

    /// Send a heartbeat to the hub.
    pub async fn heartbeat(&self, status: &NodeStatus) -> Result<()> {
        let url = format!("{}/swarm/heartbeat", self.base_url);
        let mut req = self.client.post(&url).json(status);
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::Swarm(format!(
                "heartbeat failed: {status} — {body}"
            )));
        }
        Ok(())
    }

    /// Send a task result back to the hub.
    pub async fn send_result(&self, result: &SwarmResult) -> Result<()> {
        let url = format!("{}/swarm/result", self.base_url);
        let mut req = self.client.post(&url).json(result);
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DysonError::Swarm(format!(
                "result submission failed: {status} — {body}"
            )));
        }
        Ok(())
    }

    /// Fetch a blob by SHA-256 hash from the hub.
    ///
    /// Returns the raw bytes.  Caller is responsible for verifying the
    /// hash matches what was in the signed task envelope.
    pub async fn fetch_blob(&self, sha256: &str) -> Result<Vec<u8>> {
        let url = format!("{}/swarm/blob/{sha256}", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(DysonError::Swarm(format!(
                "blob fetch failed for {sha256}: {status}"
            )));
        }
        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Upload a blob to the hub (for large result payloads).
    pub async fn upload_blob(&self, sha256: &str, data: &[u8]) -> Result<()> {
        let url = format!("{}/swarm/blob/{sha256}", self.base_url);
        let mut req = self.client.put(&url).body(data.to_vec());
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(DysonError::Swarm(format!(
                "blob upload failed for {sha256}: {status}"
            )));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Inbound: SSE event stream
    // -----------------------------------------------------------------------

    /// Open the SSE event stream from the hub.
    ///
    /// Returns a receiver that yields `SwarmEvent`s.  The SSE connection
    /// stays open until the hub closes it or the receiver is dropped.
    pub async fn open_event_stream(
        &self,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<SwarmEvent>>> {
        let url = format!("{}/swarm/events", self.base_url);
        let mut req = self.client.get(&url).header("Accept", "text/event-stream");
        if let Some(ref token) = self.auth_token {
            req = req.bearer_auth(token);
        }

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

                        // Process complete events (delimited by blank lines).
                        while let Some(event) = extract_sse_event(&mut buffer) {
                            if tx.send(Ok(event)).await.is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(DysonError::Swarm(format!(
                                "SSE stream error: {e}"
                            ))))
                            .await;
                        return;
                    }
                }
            }

            // Stream ended (hub closed connection).
            let _ = tx
                .send(Err(DysonError::Swarm(
                    "SSE stream closed by hub".into(),
                )))
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
    /// Hub-assigned node ID.
    pub node_id: String,
    /// Auth token for subsequent requests.
    pub token: String,
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

/// Try to extract one complete SSE event from the buffer.
///
/// SSE events are delimited by blank lines (`\n\n`).  Each event has
/// optional `event:` and `data:` fields.  Returns `None` if no complete
/// event is available yet.
fn extract_sse_event(buffer: &mut String) -> Option<SwarmEvent> {
    // Find the first blank-line delimiter.
    let delimiter = buffer.find("\n\n")?;

    let event_text = buffer[..delimiter].to_string();
    // Remove the event + delimiter from the buffer.
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

/// Parse an SSE event type + data into a `SwarmEvent`.
fn parse_sse_event(event_type: &str, data: &str) -> Option<SwarmEvent> {
    match event_type {
        "registered" => {
            let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
            let node_id = parsed["node_id"].as_str()?.to_string();
            Some(SwarmEvent::Registered { node_id })
        }
        "task" => {
            // Task data is base64-encoded signed wire bytes.
            let wire_bytes = STANDARD.decode(data.trim()).ok()?;
            Some(SwarmEvent::Task(wire_bytes))
        }
        "heartbeat_ack" => Some(SwarmEvent::HeartbeatAck),
        "shutdown" => Some(SwarmEvent::Shutdown),
        _ => {
            tracing::debug!(event_type, "unknown SSE event type — ignoring");
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
        let wire_data = vec![0x01u8; 70]; // fake signed bytes
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
        assert!(buf.is_empty()); // still consumed from buffer
    }

    #[test]
    fn parse_sse_incomplete_event() {
        let mut buf = "event: task\ndata: AAAA".to_string(); // no trailing \n\n
        let event = extract_sse_event(&mut buf);
        assert!(event.is_none());
        assert_eq!(buf, "event: task\ndata: AAAA"); // buffer unchanged
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
        // SSE spec: multiple `data:` lines are joined with newlines.
        let mut buf = "event: registered\ndata: {\"node_id\":\ndata:  \"abc\"}\n\n".to_string();
        let event = extract_sse_event(&mut buf).unwrap();
        match event {
            SwarmEvent::Registered { node_id } => assert_eq!(node_id, "abc"),
            _ => panic!("expected Registered"),
        }
    }
}
