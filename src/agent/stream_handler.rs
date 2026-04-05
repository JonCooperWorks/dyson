// ===========================================================================
// Stream handler — processes LLM stream events into messages and tool calls.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Consumes a `Stream<StreamEvent>` from the LLM client and produces two
//   things: (1) an assembled `Message` containing all the assistant's
//   content blocks, and (2) a list of `ToolCall` structs ready for execution.
//   Along the way, it dispatches display events to the `Output` trait so
//   the user sees the streaming response in real time.
//
// Why a separate handler instead of inline in the agent loop?
//   The agent loop is about orchestration: decide when to call the LLM,
//   when to execute tools, when to stop.  The stream handler is about
//   protocol: consume events, build content blocks, track state.  Separating
//   them keeps both small and testable.
//
// Data flow:
//
//   LLM client → Stream<Result<StreamEvent>>
//     │
//     ▼
//   process_stream()
//     │
//     ├── TextDelta("Hi")     → output.text_delta("Hi")
//     │                         → accumulate into current_text
//     ├── ToolUseStart{...}   → flush text as ContentBlock::Text
//     │                         → output.tool_use_start(...)
//     ├── ToolUseInputDelta   → (display only, accumulation in LLM client)
//     ├── ToolUseComplete{..} → ContentBlock::ToolUse + ToolCall
//     │                         → output.tool_use_complete()
//     ├── MessageComplete{..} → flush remaining text
//     └── Error(e)            → return Err(e)
//     │
//     ▼
//   Returns: (Message::assistant(blocks), Vec<ToolCall>)
// ===========================================================================

use std::pin::Pin;

use tokio_stream::Stream;
use tokio_stream::StreamExt;

use crate::controller::Output;
use crate::error::Result;
use crate::llm::stream::StreamEvent;
use crate::message::{ContentBlock, Message};

// ---------------------------------------------------------------------------
// ToolCall — a fully-formed tool call ready for execution.
// ---------------------------------------------------------------------------

/// A tool call extracted from the LLM's streaming response.
///
/// The agent loop uses these to look up the tool, run it through the
/// sandbox, execute it, and build a tool_result message.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Unique ID for this tool call (matches the LLM's tool_use block).
    ///
    /// The corresponding `tool_result` must reference this same ID so the
    /// LLM can match results to calls.
    pub id: String,

    /// Tool name (e.g., "bash").
    pub name: String,

    /// The JSON input for the tool (e.g., `{"command": "ls -la"}`).
    pub input: serde_json::Value,
}

impl ToolCall {
    /// Create a new tool call with a generated ID.
    ///
    /// Useful for tests and programmatic construction.  The ID is derived
    /// from the tool name so it's deterministic but unique enough for
    /// single-turn test scenarios.
    pub fn new(name: impl Into<String>, input: serde_json::Value) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = name.into();
        Self {
            id: format!("call_{}_{}", name, n),
            name,
            input,
        }
    }
}

// ---------------------------------------------------------------------------
// process_stream — the main entry point.
// ---------------------------------------------------------------------------

/// Consume a stream of LLM events, render them to output, and return the
/// assembled message and tool calls.
///
/// ## Parameters
///
/// - `stream`: The event stream from [`LlmClient::stream()`].
/// - `output`: Where to render events as they arrive.
///
/// ## Returns
///
/// A tuple of:
/// - `Message`: The assistant's message with all content blocks (text + tool_use).
/// - `Vec<ToolCall>`: Tool calls that need to be executed (empty if the LLM
///   just sent text and no tool calls).
/// - `usize`: Output token count (API-reported if available, otherwise estimated).
///
/// ## Error handling
///
/// If the stream emits an `Error` event or an `Err` item, this function
/// returns immediately with that error.  Partial content blocks accumulated
/// before the error are lost.
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>, usize)> {
    let mut content_blocks: Vec<ContentBlock> = Vec::with_capacity(4);
    let mut tool_calls: Vec<ToolCall> = Vec::with_capacity(4);

    // Buffer for accumulating text deltas into a single Text content block.
    let mut current_text = String::new();

    // Timing and token counting.
    let stream_start = std::time::Instant::now();
    let mut first_token_time: Option<std::time::Instant> = None;
    let mut token_count: usize = 0;
    let mut api_output_tokens: Option<usize> = None;
    let mut typing_cleared = false;

    tokio::pin!(stream);

    while let Some(event_result) = stream.next().await {
        let event = event_result?;

        match event {
            StreamEvent::ThinkingDelta(text) => {
                tracing::debug!(thinking = text, "model thinking");
            }

            StreamEvent::TextDelta(text) => {
                if first_token_time.is_none() {
                    first_token_time = Some(std::time::Instant::now());
                    let ttft_ms = first_token_time
                        .unwrap()
                        .duration_since(stream_start)
                        .as_millis();
                    tracing::info!(ttft_ms = ttft_ms, "first token received");
                }
                if !typing_cleared {
                    output.typing_indicator(false)?;
                    typing_cleared = true;
                }
                // Rough token count: split on whitespace boundaries.
                // Not exact, but good enough for tok/s estimation.
                token_count += text.split_whitespace().count().max(1);
                output.text_delta(&text)?;
                current_text.push_str(&text);
            }

            StreamEvent::ToolUseStart { ref id, ref name } => {
                if !typing_cleared {
                    output.typing_indicator(false)?;
                    typing_cleared = true;
                }
                flush_text(&mut current_text, &mut content_blocks);
                tracing::info!(tool = name, id = id, "tool call started");
                output.tool_use_start(id, name)?;
            }

            StreamEvent::ToolUseInputDelta(_) => {}

            StreamEvent::ToolUseComplete { id, name, input } => {
                if tracing::enabled!(tracing::Level::INFO) {
                    let input_str = input.to_string();
                    let input_preview = &input_str[..input_str.len().min(500)];
                    tracing::info!(
                        tool = name,
                        id = id,
                        input = input_preview,
                        "tool call complete (from stream)"
                    );
                }
                // Store in content_blocks (for the Message) and tool_calls
                // (for execution).  Clone into content_blocks, move into
                // tool_calls to avoid a second deep-copy of the input Value.
                content_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                tool_calls.push(ToolCall { id, name, input });
                output.tool_use_complete()?;
            }

            StreamEvent::MessageComplete { output_tokens, .. } => {
                api_output_tokens = output_tokens;
                let elapsed = stream_start.elapsed();
                let elapsed_ms = elapsed.as_millis();
                // Prefer the API-reported token count; fall back to the
                // rough whitespace estimate.
                let final_tokens = output_tokens.unwrap_or(token_count);
                let tok_per_sec = if elapsed.as_secs_f64() > 0.0 {
                    final_tokens as f64 / elapsed.as_secs_f64()
                } else {
                    0.0
                };
                let ttft_ms = first_token_time
                    .map(|t| t.duration_since(stream_start).as_millis())
                    .unwrap_or(0);
                tracing::info!(
                    duration_ms = elapsed_ms,
                    ttft_ms = ttft_ms,
                    tokens = final_tokens,
                    tok_per_sec = format!("{tok_per_sec:.1}"),
                    tool_calls = tool_calls.len(),
                    "stream complete"
                );
                flush_text(&mut current_text, &mut content_blocks);
            }

            StreamEvent::Error(e) => {
                return Err(e);
            }
        }
    }

    flush_text(&mut current_text, &mut content_blocks);

    let message = Message::assistant(content_blocks);
    let final_token_count = api_output_tokens.unwrap_or(token_count);
    Ok((message, tool_calls, final_token_count))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// If there's accumulated text, push it as a ContentBlock and clear the buffer.
fn flush_text(current_text: &mut String, content_blocks: &mut Vec<ContentBlock>) {
    if !current_text.is_empty() {
        content_blocks.push(ContentBlock::Text {
            text: std::mem::take(current_text),
        });
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::recording::RecordingOutput;
    use crate::error::DysonError;
    use crate::llm::stream::StopReason;

    // -----------------------------------------------------------------------
    // Helper: create a stream from a vec of events.
    // -----------------------------------------------------------------------

    fn events_to_stream(
        events: Vec<StreamEvent>,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        Box::pin(tokio_stream::iter(events.into_iter().map(Ok)))
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn text_only_response() {
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("Hello".into()),
            StreamEvent::TextDelta(" world".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, tool_calls, _tokens) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "Hello world");
        assert!(tool_calls.is_empty());
        assert_eq!(message.content.len(), 1);
        match &message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_plus_tool_call() {
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("Checking.".into()),
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, tool_calls, _tokens) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "Checking.");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "bash");
        assert_eq!(tool_calls[0].input["command"], "ls");

        // Message should have text block + tool_use block.
        assert_eq!(message.content.len(), 2);
    }

    #[tokio::test]
    async fn error_event_stops_processing() {
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("start".into()),
            StreamEvent::Error(DysonError::Llm("overloaded".into())),
        ]);

        let mut output = RecordingOutput::new();
        let result = process_stream(stream, &mut output).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn thinking_deltas_are_not_sent_to_output() {
        // Thinking tokens should be logged but NOT appear in the output
        // text or in the message content blocks.
        let stream = events_to_stream(vec![
            StreamEvent::ThinkingDelta("Let me think...".into()),
            StreamEvent::ThinkingDelta("The answer is 42.".into()),
            StreamEvent::TextDelta("The answer is 42.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, tool_calls, _tokens) = process_stream(stream, &mut output).await.unwrap();

        // Only the TextDelta should appear in output — thinking is suppressed.
        assert_eq!(output.text(), "The answer is 42.");
        assert!(tool_calls.is_empty());
        // Message should have one text block (not thinking).
        assert_eq!(message.content.len(), 1);
        match &message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

}
