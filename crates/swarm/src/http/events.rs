//! `GET /swarm/events` — long-lived SSE stream for pushing tasks to a node.
//!
//! Framing is the hand-rolled `text/event-stream` format from the spec:
//!
//! ```text
//! event: <name>\ndata: <single-line payload>\n\n
//! ```
//!
//! No `\r\n`.  No multi-line `data:` values.  The node parser in
//! `crates/dyson/src/swarm/connection.rs` terminates events on `\n\n`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::Hub;
use crate::auth::extract_bearer;
use crate::registry::SseEvent;

/// Channel depth for per-node SSE queues.
///
/// The node only processes one task at a time, so a small buffer is fine.
const SSE_CHANNEL_DEPTH: usize = 32;

pub async fn events_handler(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
) -> Response {
    // Look up the node by bearer token.
    let Some(token) = extract_bearer(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or malformed Authorization header",
        )
            .into_response();
    };

    let Some(node_id) = hub.registry.node_id_for_token(&token).await else {
        return (StatusCode::UNAUTHORIZED, "unknown bearer token").into_response();
    };

    // Build the per-node channel and attach it.
    let (tx, rx) = mpsc::channel::<SseEvent>(SSE_CHANNEL_DEPTH);
    hub.registry.attach_sse(&node_id, tx.clone()).await;

    tracing::info!(%node_id, "node SSE stream opened");

    // Prepend the initial "registered" event, then stream the channel.
    let initial = SseEvent::Registered {
        node_id: node_id.clone(),
    };

    let channel_stream = ReceiverStream::new(rx);
    let full_stream = stream::once(async move { initial }).chain(channel_stream);

    // Terminate the stream when the hub broadcasts shutdown.  Without this,
    // axum's `with_graceful_shutdown` would wait forever for the SSE stream
    // to end (it never does on its own), and Ctrl-C would appear to hang.
    let shutdown = hub.shutdown_notified();
    let byte_stream = full_stream
        .take_until(shutdown)
        .map(|event| Ok::<_, Infallible>(encode_event(&event)));

    // Remove the node from the registry as soon as it disconnects (client
    // closed the stream, network died, or the hub is shutting down).  This
    // is what the operator expects — a disconnected node should vanish
    // from `list_nodes` immediately, not linger until the reaper catches
    // up 15–90 seconds later.  Nodes reconnect by re-registering, which
    // the swarm controller already does automatically.
    let registry = hub.registry.clone();
    let detach_id = node_id.clone();
    tokio::spawn(async move {
        tx.closed().await;
        if registry.remove_node(&detach_id).await {
            tracing::info!(node_id = %detach_id, "node disconnected — removed from registry");
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(byte_stream))
        .expect("valid SSE response")
}

/// Encode an `SseEvent` as raw SSE bytes.
pub fn encode_event(event: &SseEvent) -> bytes::Bytes {
    let text = match event {
        SseEvent::Registered { node_id } => {
            let json = serde_json::json!({ "node_id": node_id });
            format!("event: registered\ndata: {json}\n\n")
        }
        SseEvent::Task(wire_bytes) => {
            let b64 = STANDARD.encode(wire_bytes);
            format!("event: task\ndata: {b64}\n\n")
        }
        SseEvent::HeartbeatAck => "event: heartbeat_ack\ndata: {}\n\n".to_string(),
        SseEvent::CancelTask(task_id) => {
            let json = serde_json::json!({ "task_id": task_id });
            format!("event: cancel_task\ndata: {json}\n\n")
        }
        SseEvent::Shutdown => "event: shutdown\ndata: {}\n\n".to_string(),
    };
    bytes::Bytes::from(text)
}

// Suppress the "unused" warning when the function is only reached via trait bounds.
#[allow(dead_code)]
const fn _assert_stream<S: Stream>() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_registered_event() {
        let e = SseEvent::Registered {
            node_id: "abc".into(),
        };
        let bytes = encode_event(&e);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: registered\n"));
        assert!(s.contains("\"node_id\":\"abc\""));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn encode_task_event_is_base64() {
        let e = SseEvent::Task(vec![0x01, 0x02, 0x03]);
        let bytes = encode_event(&e);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: task\ndata: "));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn encode_heartbeat_ack() {
        let bytes = encode_event(&SseEvent::HeartbeatAck);
        assert_eq!(&bytes[..], b"event: heartbeat_ack\ndata: {}\n\n");
    }

    #[test]
    fn encode_shutdown() {
        let bytes = encode_event(&SseEvent::Shutdown);
        assert_eq!(&bytes[..], b"event: shutdown\ndata: {}\n\n");
    }

    #[test]
    fn encode_cancel_task() {
        let bytes = encode_event(&SseEvent::CancelTask("t-123".into()));
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("event: cancel_task\n"));
        assert!(s.contains("\"task_id\":\"t-123\""));
        assert!(s.ends_with("\n\n"));
    }
}
