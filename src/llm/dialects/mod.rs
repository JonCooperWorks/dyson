// ===========================================================================
// Dialects — model-specific adaptations for tool call handling.
//
// Some models don't support the standard `tools` array in the API and
// instead emit tool calls as plain text.  Each "dialect" encapsulates
// the model-specific logic for:
//   1. Advertising tools via the system prompt
//   2. Extracting tool calls from text output
//
// The `TextToolExtractorStream` wrapper normalizes text-based tool calls
// into the same `StreamEvent` types the agent loop expects, so the rest
// of the system is unaffected.
//
// To add support for a new model family:
//   1. Create a new submodule (e.g., `llama.rs`)
//   2. Implement `TextToolHandler`
//   3. Register it in `text_tool_handler_for_model()`
// ===========================================================================

pub mod gemma;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use futures::Stream;

use crate::error::Result;
use crate::llm::stream::{StopReason, StreamEvent};
use crate::llm::ToolDefinition;

// ---------------------------------------------------------------------------
// TextToolHandler trait and types
// ---------------------------------------------------------------------------

/// A tool call extracted from model text output.
///
/// Used by [`TextToolHandler`] implementations to return parsed tool calls.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedToolCall {
    pub name: String,
    pub input: serde_json::Value,
}

/// Trait for models that don't support the standard `tools` array and instead
/// emit tool calls as plain text (e.g., Gemma's `call:name{params}` syntax).
///
/// Implementations handle two concerns:
/// - **Advertising**: injecting tool definitions into the system prompt so the
///   model knows what tools are available and how to call them.
/// - **Parsing**: extracting tool calls from the model's text output so the
///   agent loop can execute them.
///
/// To add support for a new model family, implement this trait and register
/// it in [`text_tool_handler_for_model`].
pub trait TextToolHandler: Send + Sync {
    /// Build a system prompt suffix describing the available tools.
    ///
    /// The returned string is appended to the system prompt.  It should
    /// instruct the model on the exact syntax to use for tool calls.
    fn format_tools_for_prompt(&self, tools: &[ToolDefinition]) -> String;

    /// Extract tool calls from model text output.
    ///
    /// Returns `None` if the text contains no tool calls.  Otherwise
    /// returns the cleaned text (tool call portions removed) and the
    /// extracted calls.
    fn extract_tool_calls(&self, text: &str) -> Option<(String, Vec<ExtractedToolCall>)>;
}

/// Return the appropriate [`TextToolHandler`] for a model, or `None` if the
/// model supports standard structured tool calls.
///
/// This is the single registration point — add new model families here.
pub fn text_tool_handler_for_model(model: &str) -> Option<Box<dyn TextToolHandler>> {
    if gemma::is_gemma_model(model) {
        Some(Box::new(gemma::GemmaToolHandler))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// TextToolExtractorStream
// ---------------------------------------------------------------------------

/// A stream wrapper that buffers text events and, on stream completion,
/// extracts text-based tool calls (e.g., Gemma's `call:name{params}`)
/// and emits them as proper `ToolUseComplete` events.
///
/// This keeps the extraction logic within the dialect layer —
/// `process_stream` and the agent loop see standard `StreamEvent`s.
pub(crate) struct TextToolExtractorStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    handler: Box<dyn TextToolHandler>,
    /// Buffered text accumulated from TextDelta events.
    text_buffer: String,
    /// Events buffered from the inner stream, waiting to be emitted.
    buffered_events: Vec<StreamEvent>,
    /// Synthetic events generated from extracted tool calls.
    pending_events: VecDeque<Result<StreamEvent>>,
    /// Whether the inner stream has completed.
    inner_done: bool,
    /// Whether we found any structured tool calls (skip extraction if so).
    has_structured_tools: bool,
}

impl TextToolExtractorStream {
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
        handler: Box<dyn TextToolHandler>,
    ) -> Self {
        Self {
            inner,
            handler,
            text_buffer: String::new(),
            buffered_events: Vec::new(),
            pending_events: VecDeque::new(),
            inner_done: false,
            has_structured_tools: false,
        }
    }

    /// Process the completed stream: extract tool calls from text and
    /// generate synthetic events.
    fn finalize(&mut self, output_tokens: Option<usize>) {
        if self.has_structured_tools || self.text_buffer.is_empty() {
            // Re-emit buffered events as-is, plus the MessageComplete.
            for event in self.buffered_events.drain(..) {
                self.pending_events.push_back(Ok(event));
            }
            self.pending_events
                .push_back(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens,
                }));
            return;
        }

        if let Some((cleaned, calls)) = self.handler.extract_tool_calls(&self.text_buffer) {
            // Emit cleaned text (if any).
            if !cleaned.is_empty() {
                self.pending_events
                    .push_back(Ok(StreamEvent::TextDelta(cleaned)));
            }

            // Emit synthetic tool call events.
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            for call in &calls {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let id = format!("text_call_{}_{}", call.name, n);
                self.pending_events
                    .push_back(Ok(StreamEvent::ToolUseStart {
                        id: id.clone(),
                        name: call.name.clone(),
                    }));
                self.pending_events
                    .push_back(Ok(StreamEvent::ToolUseComplete {
                        id,
                        name: call.name.clone(),
                        input: call.input.clone(),
                    }));
            }

            // Change stop reason to ToolUse.
            self.pending_events
                .push_back(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens,
                }));
        } else {
            // No tool calls found — re-emit everything as-is.
            for event in self.buffered_events.drain(..) {
                self.pending_events.push_back(Ok(event));
            }
            self.pending_events
                .push_back(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens,
                }));
        }
    }
}

impl Stream for TextToolExtractorStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Drain pending events first.
        if !this.pending_events.is_empty() {
            return Poll::Ready(Some(this.pending_events.pop_front().unwrap()));
        }

        if this.inner_done {
            return Poll::Ready(None);
        }

        // Poll the inner stream.
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => match event {
                StreamEvent::TextDelta(ref text) => {
                    this.text_buffer.push_str(text);
                    this.buffered_events.push(event);
                    // Don't emit yet — we need to wait for completion
                    // to know if we should extract tool calls.
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                StreamEvent::ToolUseStart { .. }
                | StreamEvent::ToolUseComplete { .. }
                | StreamEvent::ToolUseInputDelta(_) => {
                    this.has_structured_tools = true;
                    // Flush any buffered text events first.
                    for buffered in this.buffered_events.drain(..) {
                        this.pending_events.push_back(Ok(buffered));
                    }
                    this.pending_events.push_back(Ok(event));
                    Poll::Ready(Some(this.pending_events.pop_front().unwrap()))
                }
                StreamEvent::MessageComplete { output_tokens, .. } => {
                    this.inner_done = true;
                    this.finalize(output_tokens);
                    if this.pending_events.is_empty() {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(this.pending_events.pop_front().unwrap()))
                    }
                }
                // ThinkingDelta, Error — pass through immediately.
                other => {
                    this.buffered_events.push(other);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            Poll::Ready(Some(Err(e))) => {
                this.inner_done = true;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.inner_done = true;
                // Stream ended without MessageComplete — finalize anyway.
                this.finalize(None);
                if this.pending_events.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(this.pending_events.pop_front().unwrap()))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::dialects::gemma::GemmaToolHandler;
    use tokio_stream::StreamExt;

    fn mock_stream(
        events: Vec<StreamEvent>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        Box::pin(tokio_stream::iter(events.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn extracts_gemma_tool_calls_from_text() {
        let inner = mock_stream(vec![
            StreamEvent::TextDelta("Let me check.\ncall:bash{command: 'ls -la'}".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let handler: Box<dyn TextToolHandler> = Box::new(GemmaToolHandler);
        let mut stream = TextToolExtractorStream::new(inner, handler);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }

        // Should have: cleaned text, tool start, tool complete, message complete.
        let has_text = events.iter().any(|e| {
            matches!(e, StreamEvent::TextDelta(t) if t == "Let me check.")
        });
        assert!(has_text, "cleaned text should be emitted");

        let has_tool = events.iter().any(|e| {
            matches!(e, StreamEvent::ToolUseComplete { name, input, .. }
                if name == "bash" && input["command"] == "ls -la")
        });
        assert!(has_tool, "tool call should be extracted");

        let has_tool_stop = events.iter().any(|e| {
            matches!(e, StreamEvent::MessageComplete { stop_reason: StopReason::ToolUse, .. })
        });
        assert!(has_tool_stop, "stop reason should be ToolUse");
    }

    #[tokio::test]
    async fn passthrough_when_structured_tools_present() {
        let inner = mock_stream(vec![
            StreamEvent::TextDelta("call:bash{command: 'ls'}".into()),
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "pwd"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ]);

        let handler: Box<dyn TextToolHandler> = Box::new(GemmaToolHandler);
        let mut stream = TextToolExtractorStream::new(inner, handler);

        let mut tool_completes = Vec::new();
        while let Some(event) = stream.next().await {
            if let Ok(StreamEvent::ToolUseComplete { name, input, .. }) = &event {
                tool_completes.push((name.clone(), input.clone()));
            }
        }

        // Only the structured tool call — no duplicate extraction from text.
        assert_eq!(tool_completes.len(), 1);
        assert_eq!(tool_completes[0].1["command"], "pwd");
    }

    #[tokio::test]
    async fn no_tool_calls_passes_through() {
        let inner = mock_stream(vec![
            StreamEvent::TextDelta("Hello world".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let handler: Box<dyn TextToolHandler> = Box::new(GemmaToolHandler);
        let mut stream = TextToolExtractorStream::new(inner, handler);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }

        let has_text = events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "Hello world"));
        assert!(has_text);

        let has_end = events.iter().any(|e| {
            matches!(e, StreamEvent::MessageComplete { stop_reason: StopReason::EndTurn, .. })
        });
        assert!(has_end);
    }
}
