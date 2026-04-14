// ===========================================================================
// SSE parser base — shared tool buffer management for SSE-based LLM clients.
//
// Both Anthropic and OpenAI SSE parsers need identical logic for:
//   - Wrapping SseLineBuffer for SSE framing
//   - Managing tool call accumulation buffers (HashMap<usize, ToolCallBuffer>)
//   - Tracking thinking block indices
//   - Size guards (MAX_ACTIVE_TOOL_BUFFERS, MAX_TOOL_JSON)
//   - The feed() → parse payloads → collect events loop
//
// This module provides `BaseSseParser<P>` which handles all of the above.
// Each provider implements the `SseJsonParser` trait for its specific
// JSON → StreamEvent mapping.
// ===========================================================================

use std::collections::{HashMap, HashSet};

use crate::error::{DysonError, Result};
use crate::llm::stream::StreamEvent;
use crate::llm::{
    SseLineBuffer, SseStreamParser, ToolCallBuffer,
    MAX_ACTIVE_TOOL_BUFFERS, MAX_TOOL_JSON, finalize_tool_call,
};

/// Provider-specific SSE JSON parsing.
///
/// Implementors receive the parsed JSON payload and a mutable reference
/// to the shared tool buffer state.  They return zero or more `StreamEvent`s.
pub trait SseJsonParser {
    /// Parse a single SSE JSON payload into zero or more StreamEvents.
    ///
    /// The parser has access to the shared tool buffers via the `ctx`
    /// parameter for tool call accumulation.
    fn parse_json(
        &mut self,
        json: &serde_json::Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>>;
}

/// Shared mutable state for tool call accumulation during SSE parsing.
///
/// Both Anthropic and OpenAI parsers need to:
/// - Start tool buffers on tool_use/tool_call start events
/// - Append partial JSON during delta events
/// - Finalize tool calls on stop/finish events
/// - Track thinking block indices
///
/// This struct provides those operations with built-in size guards.
pub struct ToolBufferContext {
    /// Active tool_use blocks being accumulated (keyed by content block index).
    pub(crate) tool_buffers: HashMap<usize, ToolCallBuffer>,
    /// Content block indices that are "thinking" blocks.
    pub(crate) thinking_blocks: HashSet<usize>,
}

impl ToolBufferContext {
    fn new() -> Self {
        Self {
            tool_buffers: HashMap::new(),
            thinking_blocks: HashSet::new(),
        }
    }

    /// Start accumulating a new tool call buffer.
    ///
    /// Returns `Some(error event)` if the buffer limit is exceeded.
    pub(crate) fn start_tool(
        &mut self,
        index: usize,
        id: String,
        name: String,
    ) -> Option<StreamEvent> {
        if self.tool_buffers.len() >= MAX_ACTIVE_TOOL_BUFFERS {
            return Some(StreamEvent::Error(DysonError::Llm(format!(
                "too many concurrent tool calls ({MAX_ACTIVE_TOOL_BUFFERS}) — aborting stream"
            ))));
        }
        self.tool_buffers.insert(
            index,
            ToolCallBuffer {
                id,
                name,
                json: String::new(),
            },
        );
        None
    }

    /// Append partial JSON to an existing tool buffer.
    ///
    /// Returns `Some(error event)` if the accumulated JSON exceeds the size limit.
    pub(crate) fn append_tool_json(
        &mut self,
        index: usize,
        partial: &str,
    ) -> Option<StreamEvent> {
        if let Some(buf) = self.tool_buffers.get_mut(&index) {
            if buf.json.len() + partial.len() > MAX_TOOL_JSON {
                // Remove the buffer to prevent repeated error events and
                // free the accumulated memory.
                self.tool_buffers.remove(&index);
                return Some(StreamEvent::TextDelta(
                    "[error: tool input exceeded 10 MB limit]".into(),
                ));
            }
            buf.json.push_str(partial);
        }
        None
    }

    /// Finalize a tool call buffer at the given index.
    ///
    /// Returns `Some(ToolUseComplete event)` if the index had an active buffer.
    pub(crate) fn finalize_tool(&mut self, index: usize) -> Option<Result<StreamEvent>> {
        self.tool_buffers.remove(&index).map(finalize_tool_call)
    }

    /// Drain all remaining tool buffers, returning ToolUseComplete events.
    pub(crate) fn drain_all(&mut self) -> Vec<Result<StreamEvent>> {
        self.tool_buffers
            .drain()
            .map(|(_, buf)| finalize_tool_call(buf))
            .collect()
    }
}

/// Generic SSE parser that combines shared infrastructure with a
/// provider-specific JSON parser.
///
/// Implements `SseStreamParser` so it can be used with `sse_event_stream()`.
pub struct BaseSseParser<P: SseJsonParser> {
    line_buffer: SseLineBuffer,
    pub(crate) ctx: ToolBufferContext,
    parser: P,
}

impl<P: SseJsonParser> BaseSseParser<P> {
    pub(crate) fn new(parser: P) -> Self {
        Self {
            line_buffer: SseLineBuffer::new(),
            ctx: ToolBufferContext::new(),
            parser,
        }
    }
}

impl<P: SseJsonParser + Send + 'static> SseStreamParser for BaseSseParser<P> {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Result<StreamEvent>> {
        let mut events = Vec::new();

        let payloads = match self.line_buffer.feed(bytes) {
            Ok(p) => p,
            Err(e) => {
                events.push(Err(e));
                return events;
            }
        };

        for data in payloads {
            if data == "[DONE]" {
                // Flush any remaining tool buffers.
                events.extend(self.ctx.drain_all());
                continue;
            }

            match serde_json::from_str::<serde_json::Value>(&data) {
                Ok(json) => {
                    let new_events = self.parser.parse_json(&json, &mut self.ctx);
                    events.extend(new_events);
                }
                Err(e) => {
                    tracing::warn!(data = data, error = %e, "failed to parse SSE JSON");
                }
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_buffer_removed_after_overflow() {
        let mut ctx = ToolBufferContext::new();
        ctx.start_tool(0, "id".into(), "name".into());

        // Append data that exceeds MAX_TOOL_JSON.
        let big = "x".repeat(MAX_TOOL_JSON + 1);
        let event = ctx.append_tool_json(0, &big);
        assert!(event.is_some(), "should return error event on overflow");

        // Buffer should be removed so subsequent appends are harmless no-ops.
        assert!(
            !ctx.tool_buffers.contains_key(&0),
            "buffer should be removed after overflow"
        );

        // Subsequent append to the same index should be a no-op.
        let event2 = ctx.append_tool_json(0, "more");
        assert!(event2.is_none(), "no buffer means no error event");
    }
}
