// ===========================================================================
// SSE Output — streams agent output as Server-Sent Events.
//
// Implements the Output trait by sending JSON events through an mpsc channel.
// The HTTP handler consumes the channel as an SSE byte stream.
//
// Event format (one JSON object per SSE `data:` line):
//   event: text_delta       data: {"text": "..."}
//   event: tool_use_start   data: {"id": "...", "name": "..."}
//   event: tool_result      data: {"content": "...", "is_error": false}
//   event: error            data: {"message": "..."}
//   event: done             data: {}
// ===========================================================================

use std::path::Path;

use tokio::sync::mpsc;

use crate::controller::Output;
use crate::error::DysonError;
use crate::tool::ToolOutput;

/// An SSE event to be serialized and sent to the HTTP client.
#[derive(Debug)]
pub(super) struct SseEvent {
    pub event: &'static str,
    pub data: serde_json::Value,
}

impl SseEvent {
    /// Format as an SSE wire line: `event: {event}\ndata: {json}\n\n`
    pub fn to_sse_bytes(&self) -> Vec<u8> {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_else(|_| "{}".into()),
        )
        .into_bytes()
    }
}

/// Output implementation that sends events through an mpsc channel.
///
/// The web controller's HTTP handler holds the receiver end and streams
/// events to the client as SSE.
pub(super) struct SseOutput {
    tx: mpsc::Sender<SseEvent>,
}

impl SseOutput {
    pub fn new(tx: mpsc::Sender<SseEvent>) -> Self {
        Self { tx }
    }

    /// Send an event, ignoring channel-closed errors (client disconnected).
    fn send(&self, event: &'static str, data: serde_json::Value) {
        let _ = self.tx.try_send(SseEvent { event, data });
    }
}

impl Output for SseOutput {
    fn text_delta(&mut self, text: &str) -> Result<(), DysonError> {
        self.send("text_delta", serde_json::json!({"text": text}));
        Ok(())
    }

    fn tool_use_start(&mut self, id: &str, name: &str) -> Result<(), DysonError> {
        self.send(
            "tool_use_start",
            serde_json::json!({"id": id, "name": name}),
        );
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        // No separate event — tool_result signals completion.
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<(), DysonError> {
        self.send(
            "tool_result",
            serde_json::json!({
                "content": output.content,
                "is_error": output.is_error,
            }),
        );
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<(), DysonError> {
        self.send(
            "tool_result",
            serde_json::json!({
                "content": format!("[file: {}]", path.display()),
                "is_error": false,
            }),
        );
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        self.send("error", serde_json::json!({"message": error.to_string()}));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        // SSE events are sent immediately — no buffering to flush.
        Ok(())
    }
}
