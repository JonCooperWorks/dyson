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

use futures::Stream;
use tokio_stream::StreamExt;

use crate::error::Result;
use crate::llm::stream::StreamEvent;
use crate::message::{ContentBlock, Message};
use crate::controller::Output;

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
///
/// ## Error handling
///
/// If the stream emits an `Error` event or an `Err` item, this function
/// returns immediately with that error.  Partial content blocks accumulated
/// before the error are lost.
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>)> {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    // Buffer for accumulating text deltas into a single Text content block.
    //
    // Text arrives as many small fragments ("Hello", " ", "world", "!").
    // We concatenate them into one string and flush it as a ContentBlock
    // when we hit a non-text event or the stream ends.
    let mut current_text = String::new();

    tokio::pin!(stream);

    while let Some(event_result) = stream.next().await {
        let event = event_result?;

        match event {
            // -- Thinking fragment --
            //
            // Models with extended thinking (Claude, o-series, DeepSeek, etc.)
            // emit reasoning tokens before the visible response.  We log them
            // for debugging but don't surface them to the user.
            StreamEvent::ThinkingDelta(text) => {
                tracing::debug!(thinking = text, "model thinking");
            }

            // -- Text fragment --
            StreamEvent::TextDelta(text) => {
                output.text_delta(&text)?;
                current_text.push_str(&text);
            }

            // -- Tool call starting --
            //
            // Flush any accumulated text as a content block before the
            // tool_use block.  This preserves the ordering: text blocks
            // and tool_use blocks interleave correctly.
            StreamEvent::ToolUseStart { ref id, ref name } => {
                flush_text(&mut current_text, &mut content_blocks);
                output.tool_use_start(id, name)?;
            }

            // -- Partial tool input JSON --
            //
            // The real accumulation happens inside the AnthropicClient's
            // SSE parser.  We receive these for logging/display only.
            StreamEvent::ToolUseInputDelta(_) => {
                // Nothing to do — the LLM client accumulates internally.
            }

            // -- Tool call fully formed --
            //
            // Add the tool_use content block and record the call for
            // the agent loop to execute.
            StreamEvent::ToolUseComplete { id, name, input } => {
                content_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                tool_calls.push(ToolCall { id, name, input });
                output.tool_use_complete()?;
            }

            // -- Message complete --
            //
            // The LLM is done generating.  Flush any remaining text.
            StreamEvent::MessageComplete { .. } => {
                flush_text(&mut current_text, &mut content_blocks);
            }

            // -- Stream error --
            StreamEvent::Error(e) => {
                return Err(e);
            }
        }
    }

    // In case the stream ended without a MessageComplete event
    // (shouldn't happen with well-behaved APIs, but defensive coding).
    flush_text(&mut current_text, &mut content_blocks);

    let message = Message::assistant(content_blocks);
    Ok((message, tool_calls))
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
    use crate::error::DysonError;
    use crate::llm::stream::StopReason;

    // -----------------------------------------------------------------------
    // Mock output that records events for assertion.
    // -----------------------------------------------------------------------

    struct MockOutput {
        text: String,
        tool_starts: Vec<(String, String)>,
        tool_completes: usize,
    }

    impl MockOutput {
        fn new() -> Self {
            Self {
                text: String::new(),
                tool_starts: Vec::new(),
                tool_completes: 0,
            }
        }
    }

    impl Output for MockOutput {
        fn text_delta(&mut self, text: &str) -> Result<()> {
            self.text.push_str(text);
            Ok(())
        }
        fn tool_use_start(&mut self, id: &str, name: &str) -> Result<()> {
            self.tool_starts.push((id.into(), name.into()));
            Ok(())
        }
        fn tool_use_complete(&mut self) -> Result<()> {
            self.tool_completes += 1;
            Ok(())
        }
        fn tool_result(&mut self, _: &crate::tool::ToolOutput) -> Result<()> {
            Ok(())
        }
        fn error(&mut self, _: &DysonError) -> Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

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
            },
        ]);

        let mut output = MockOutput::new();
        let (message, tool_calls) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text, "Hello world");
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
            },
        ]);

        let mut output = MockOutput::new();
        let (message, tool_calls) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text, "Checking.");
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

        let mut output = MockOutput::new();
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
            },
        ]);

        let mut output = MockOutput::new();
        let (message, tool_calls) = process_stream(stream, &mut output).await.unwrap();

        // Only the TextDelta should appear in output — thinking is suppressed.
        assert_eq!(output.text, "The answer is 42.");
        assert!(tool_calls.is_empty());
        // Message should have one text block (not thinking).
        assert_eq!(message.content.len(), 1);
        match &message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }
}
