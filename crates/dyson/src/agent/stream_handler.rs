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
use crate::llm::stream::{StopReason, StreamEvent};
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
/// - `StopReason`: Why the LLM stopped generating (end_turn, tool_use, or
///   max_tokens).  The agent loop uses this to detect truncated responses
///   and inject continuation prompts.
///
/// ## Error handling
///
/// If the stream emits an `Error` event or an `Err` item, this function
/// returns immediately with that error.  Partial content blocks accumulated
/// before the error are lost.
pub async fn process_stream(
    stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    output: &mut dyn Output,
) -> Result<(Message, Vec<ToolCall>, usize, StopReason)> {
    let mut content_blocks: Vec<ContentBlock> = Vec::with_capacity(4);
    let mut tool_calls: Vec<ToolCall> = Vec::with_capacity(4);

    // Buffer for accumulating text deltas into a single Text content block.
    let mut current_text = String::new();

    // Buffer for accumulating thinking deltas into a Thinking content block.
    let mut current_thinking = String::new();
    let mut thinking_indicator_sent = false;

    // Detects <think>…</think> tags in TextDelta events from OpenAI-compat
    // servers and reclassifies them as thinking content.
    let mut think_tag_parser = ThinkTagParser::new();

    // Timing, token counting, and stop reason tracking.
    let stream_start = std::time::Instant::now();
    let mut first_token_time: Option<std::time::Instant> = None;
    let mut token_count: usize = 0;
    let mut api_output_tokens: Option<usize> = None;
    let mut typing_cleared = false;
    let mut final_stop_reason = StopReason::EndTurn;

    tokio::pin!(stream);

    while let Some(event_result) = stream.next().await {
        let event = event_result?;

        match event {
            StreamEvent::ThinkingDelta(text) => {
                tracing::debug!(thinking = text, "model thinking");
                // Send a one-time "thinking" indicator so the user knows
                // the model is reasoning, but don't reveal the full text.
                if !thinking_indicator_sent {
                    if !typing_cleared {
                        output.typing_indicator(false)?;
                        typing_cleared = true;
                    }
                    output.text_delta("I'm thinking …\n")?;
                    thinking_indicator_sent = true;
                }
                current_thinking.push_str(&text);
            }

            StreamEvent::TextDelta(text) => {
                // Run through the <think> tag parser — some OpenAI-compat
                // servers embed reasoning in <think>…</think> tags in the
                // content field rather than using a separate field.
                let segments = think_tag_parser.feed(&text);
                for (is_thinking, segment) in segments {
                    if is_thinking {
                        tracing::debug!(thinking = segment, "model thinking (think tag)");
                        if !thinking_indicator_sent {
                            if !typing_cleared {
                                output.typing_indicator(false)?;
                                typing_cleared = true;
                            }
                            output.text_delta("I'm thinking …\n")?;
                            thinking_indicator_sent = true;
                        }
                        current_thinking.push_str(&segment);
                    } else {
                        // Flush any accumulated thinking before text starts.
                        flush_thinking(&mut current_thinking, &mut content_blocks);
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
                        token_count += segment.split_whitespace().count().max(1);
                        output.text_delta(&segment)?;
                        current_text.push_str(&segment);
                    }
                }
            }

            StreamEvent::ToolUseStart { ref id, ref name } => {
                if !typing_cleared {
                    output.typing_indicator(false)?;
                    typing_cleared = true;
                }
                flush_thinking(&mut current_thinking, &mut content_blocks);
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

            StreamEvent::MessageComplete { stop_reason, output_tokens } => {
                final_stop_reason = stop_reason;
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
                for (is_thinking, segment) in think_tag_parser.flush() {
                    if is_thinking {
                        current_thinking.push_str(&segment);
                    } else {
                        output.text_delta(&segment)?;
                        current_text.push_str(&segment);
                    }
                }
                flush_thinking(&mut current_thinking, &mut content_blocks);
                flush_text(&mut current_text, &mut content_blocks);
            }

            StreamEvent::Error(e) => {
                return Err(e);
            }
        }
    }

    // Flush any remaining content held by the think tag parser.
    for (is_thinking, segment) in think_tag_parser.flush() {
        if is_thinking {
            current_thinking.push_str(&segment);
        } else {
            output.text_delta(&segment)?;
            current_text.push_str(&segment);
        }
    }
    flush_thinking(&mut current_thinking, &mut content_blocks);
    flush_text(&mut current_text, &mut content_blocks);

    let message = Message::assistant(content_blocks);
    let final_token_count = api_output_tokens.unwrap_or(token_count);
    Ok((message, tool_calls, final_token_count, final_stop_reason))
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

/// If there's accumulated thinking, push it as a Thinking ContentBlock and clear the buffer.
fn flush_thinking(current_thinking: &mut String, content_blocks: &mut Vec<ContentBlock>) {
    if !current_thinking.is_empty() {
        content_blocks.push(ContentBlock::Thinking {
            thinking: std::mem::take(current_thinking),
        });
    }
}

// ---------------------------------------------------------------------------
// ThinkTagParser — extracts <think>…</think> from streamed text deltas.
// ---------------------------------------------------------------------------

/// Some OpenAI-compatible inference servers (vLLM, llama.cpp, Ollama, etc.)
/// wrap model reasoning in `<think>…</think>` tags at the start of the
/// response.  This parser detects those tags in a streaming context where
/// the tag boundaries can be split across arbitrary delta chunks.
///
/// Usage: feed every `TextDelta` through [`feed()`].  It returns a list of
/// `(is_thinking, text)` segments.
/// Maximum pending buffer size before forcing a flush.  Protects against
/// unbounded memory growth when a model sends a giant chunk inside an
/// unclosed `<think>` tag.  In steady state the buffer holds at most
/// `CLOSE.len() - 1` bytes between calls (we emit everything except the
/// possible partial `</think>` suffix), so this cap only matters for
/// single oversized deltas.  8 KB is more than enough headroom while
/// keeping per-stream worst case small when many streams run concurrently.
const MAX_PENDING: usize = 8 * 1024; // 8 KB

struct ThinkTagParser {
    state: ThinkTagState,
    /// Bytes buffered while we're waiting to see if a partial match
    /// completes (e.g. we've seen `<thi` and need more bytes).
    pending: String,
}

#[derive(Debug, PartialEq)]
enum ThinkTagState {
    /// Haven't decided yet — looking for `<think>` at the very start.
    Start,
    /// Inside `<think>…</think>`, accumulating thinking text.
    /// Watching for `</think>`.
    InsideThink,
    /// Past the think block (or there was none).  Pass text through.
    PassThrough,
}

impl ThinkTagParser {
    const fn new() -> Self {
        Self {
            state: ThinkTagState::Start,
            pending: String::new(),
        }
    }

    /// Feed a text delta chunk.  Returns segments of `(is_thinking, text)`.
    fn feed(&mut self, text: &str) -> Vec<(bool, String)> {
        let mut out = Vec::new();
        match self.state {
            ThinkTagState::Start => {
                self.pending.push_str(text);
                const OPEN: &str = "<think>";
                if self.pending.len() >= OPEN.len() {
                    if self.pending.starts_with(OPEN) {
                        // Confirmed opening tag — switch to inside.
                        self.state = ThinkTagState::InsideThink;
                        let rest = self.pending[OPEN.len()..].to_string();
                        self.pending.clear();
                        if !rest.is_empty() {
                            out.extend(self.feed(&rest));
                        }
                    } else {
                        // Not a think tag — flush everything as text.
                        self.state = ThinkTagState::PassThrough;
                        let flushed = std::mem::take(&mut self.pending);
                        out.push((false, flushed));
                    }
                }
                // else: still accumulating, might be a prefix of "<think>"
                // (e.g. "<thi"), keep waiting.
            }

            ThinkTagState::InsideThink => {
                self.pending.push_str(text);

                // Guard against unbounded growth from an unclosed <think> tag.
                if self.pending.len() > MAX_PENDING {
                    tracing::warn!(
                        buffered = self.pending.len(),
                        "think tag buffer exceeded {MAX_PENDING} bytes, flushing as thinking text"
                    );
                    let flushed = std::mem::take(&mut self.pending);
                    self.state = ThinkTagState::PassThrough;
                    out.push((true, flushed));
                    return out;
                }

                const CLOSE: &str = "</think>";
                if let Some(pos) = self.pending.find(CLOSE) {
                    // Found closing tag.
                    let thinking = self.pending[..pos].to_string();
                    let after = self.pending[pos + CLOSE.len()..].to_string();
                    self.pending.clear();
                    self.state = ThinkTagState::PassThrough;
                    if !thinking.is_empty() {
                        out.push((true, thinking));
                    }
                    if !after.is_empty() {
                        out.push((false, after));
                    }
                } else {
                    // No closing tag yet.  Emit everything except the last
                    // few bytes which could be a partial "</think>" match.
                    let safe = self.pending.len().saturating_sub(CLOSE.len() - 1);
                    if safe > 0 {
                        let emit = self.pending[..safe].to_string();
                        self.pending = self.pending[safe..].to_string();
                        out.push((true, emit));
                    }
                }
            }

            ThinkTagState::PassThrough => {
                out.push((false, text.to_string()));
            }
        }
        out
    }

    /// Flush any remaining buffered content at end of stream.
    fn flush(&mut self) -> Vec<(bool, String)> {
        let mut out = Vec::new();
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            match self.state {
                ThinkTagState::Start => {
                    // Never matched <think> — emit as regular text.
                    out.push((false, text));
                }
                ThinkTagState::InsideThink => {
                    // Unclosed <think> — still save as thinking.
                    out.push((true, text));
                }
                ThinkTagState::PassThrough => {
                    out.push((false, text));
                }
            }
        }
        self.state = ThinkTagState::PassThrough;
        out
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
        let (message, tool_calls, _tokens, stop) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "Hello world");
        assert!(tool_calls.is_empty());
        assert_eq!(stop, StopReason::EndTurn);
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
        let (message, tool_calls, _tokens, stop) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "Checking.");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "bash");
        assert_eq!(tool_calls[0].input["command"], "ls");
        assert_eq!(stop, StopReason::ToolUse);

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
    async fn thinking_deltas_saved_to_history_but_hidden_from_output() {
        // Thinking tokens should be saved to the message content blocks
        // for history, but only a brief indicator shown to the user.
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
        let (message, tool_calls, _tokens, _stop) = process_stream(stream, &mut output).await.unwrap();

        // Output should show the thinking indicator + the visible text.
        assert_eq!(output.text(), "I'm thinking …\nThe answer is 42.");
        assert!(tool_calls.is_empty());
        // Message should have a Thinking block + a Text block.
        assert_eq!(message.content.len(), 2);
        match &message.content[0] {
            ContentBlock::Thinking { thinking } => {
                assert_eq!(thinking, "Let me think...The answer is 42.");
            }
            other => panic!("expected Thinking, got: {other:?}"),
        }
        match &message.content[1] {
            ContentBlock::Text { text } => assert_eq!(text, "The answer is 42."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // ThinkTagParser unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn think_tag_single_chunk() {
        let mut p = ThinkTagParser::new();
        let segs = p.feed("<think>reasoning here</think>visible text");
        let mut all = segs;
        all.extend(p.flush());
        assert_eq!(all, vec![
            (true, "reasoning here".to_string()),
            (false, "visible text".to_string()),
        ]);
    }

    #[test]
    fn think_tag_split_across_chunks() {
        let mut p = ThinkTagParser::new();
        let mut all = Vec::new();
        // Opening tag split across two chunks.
        all.extend(p.feed("<thi"));
        all.extend(p.feed("nk>deep thought</"));
        all.extend(p.feed("think>hello"));
        all.extend(p.flush());

        let thinking: String = all.iter()
            .filter(|(t, _)| *t)
            .map(|(_, s)| s.as_str())
            .collect();
        let text: String = all.iter()
            .filter(|(t, _)| !*t)
            .map(|(_, s)| s.as_str())
            .collect();
        assert_eq!(thinking, "deep thought");
        assert_eq!(text, "hello");
    }

    #[test]
    fn think_tag_no_tag_passes_through() {
        let mut p = ThinkTagParser::new();
        let mut all = Vec::new();
        all.extend(p.feed("Hello world"));
        all.extend(p.flush());
        assert_eq!(all, vec![(false, "Hello world".to_string())]);
    }

    #[test]
    fn think_tag_short_non_matching_prefix() {
        // Text that starts with "<" but is not "<think>".
        let mut p = ThinkTagParser::new();
        let mut all = Vec::new();
        all.extend(p.feed("<b>bold</b>"));
        all.extend(p.flush());
        assert_eq!(all, vec![(false, "<b>bold</b>".to_string())]);
    }

    #[test]
    fn think_tag_buffered_prefix_flushed_at_end() {
        // Only receive a partial prefix then stream ends.
        let mut p = ThinkTagParser::new();
        let mut all = Vec::new();
        all.extend(p.feed("<thi"));
        all.extend(p.flush());
        // Should emit as regular text since it never completed.
        assert_eq!(all, vec![(false, "<thi".to_string())]);
    }

    // -----------------------------------------------------------------------
    // process_stream with <think> tags (OpenAI-compat)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn think_tags_in_text_deltas_extracted_as_thinking() {
        // Simulates an OpenAI-compat server that sends <think>…</think>
        // in the content field.
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("<think>".into()),
            StreamEvent::TextDelta("Let me reason.".into()),
            StreamEvent::TextDelta("</think>".into()),
            StreamEvent::TextDelta("Final answer.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, _, _, _stop) = process_stream(stream, &mut output).await.unwrap();

        // Output should show indicator + visible text only.
        assert_eq!(output.text(), "I'm thinking …\nFinal answer.");

        // Message should have Thinking + Text blocks.
        assert_eq!(message.content.len(), 2);
        match &message.content[0] {
            ContentBlock::Thinking { thinking } => {
                assert_eq!(thinking, "Let me reason.");
            }
            other => panic!("expected Thinking, got: {other:?}"),
        }
        match &message.content[1] {
            ContentBlock::Text { text } => assert_eq!(text, "Final answer."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn think_tags_all_in_one_chunk() {
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("<think>reasoning</think>answer".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, _, _, _stop) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "I'm thinking …\nanswer");
        assert_eq!(message.content.len(), 2);
        match &message.content[0] {
            ContentBlock::Thinking { thinking } => assert_eq!(thinking, "reasoning"),
            other => panic!("expected Thinking, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_think_tags_passes_through() {
        // Regular text without think tags should work as before.
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("Just normal text.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ]);

        let mut output = RecordingOutput::new();
        let (message, _, _, _stop) = process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "Just normal text.");
        assert_eq!(message.content.len(), 1);
        match &message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Just normal text."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_tokens_stop_reason_propagated() {
        // When the LLM hits the token limit, the stop reason should be
        // MaxTokens so the agent loop can inject a continuation prompt.
        let stream = events_to_stream(vec![
            StreamEvent::TextDelta("This response is trunca".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::MaxTokens,
                output_tokens: Some(8192),
            },
        ]);

        let mut output = RecordingOutput::new();
        let (_message, tool_calls, _tokens, stop_reason) =
            process_stream(stream, &mut output).await.unwrap();

        assert_eq!(output.text(), "This response is trunca");
        assert!(tool_calls.is_empty());
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

}
