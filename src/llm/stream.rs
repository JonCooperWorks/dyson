// ===========================================================================
// Stream events — the vocabulary of an LLM response in flight.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines `StreamEvent` and `StopReason` — the types that represent
//   individual chunks of an LLM's streaming response.  When the LLM
//   generates a response, it doesn't arrive all at once.  Instead, it
//   arrives as a sequence of events: text fragments, tool call starts,
//   tool input fragments, and completion signals.
//
// Why stream instead of buffering?
//   Streaming is core to Dyson's UX.  Text appears on the user's terminal
//   as the LLM generates it — no waiting for the full response.  Tool calls
//   are detected as they complete, not after the entire message is done.
//   This makes the agent feel responsive even on long generations.
//
// How events flow:
//
//   Anthropic SSE → AnthropicClient parses → Stream<StreamEvent>
//     │
//     ▼
//   stream_handler consumes events:
//     TextDelta("Hello")       → print "Hello" to terminal
//     TextDelta(" world")      → print " world" to terminal
//     ToolUseStart{id, name}   → print "[Tool: bash]"
//     ToolUseInputDelta(json)  → (accumulated inside AnthropicClient)
//     ToolUseComplete{...}     → dispatch tool execution
//     MessageComplete{stop}    → end of this LLM turn
//
// The ToolUseComplete event is synthetic — it's not a direct Anthropic SSE
// event.  The AnthropicClient accumulates ToolUseInputDelta fragments and
// emits ToolUseComplete when the content_block_stop event arrives.  This
// keeps the stream handler simple: it just pattern-matches on events.
// ===========================================================================

use crate::error::DysonError;

// ---------------------------------------------------------------------------
// StopReason
// ---------------------------------------------------------------------------

/// Why the LLM stopped generating.
///
/// This determines what the agent loop does next:
/// - `EndTurn` + no tool calls → conversation turn is done
/// - `ToolUse` → execute tools, then loop (LLM sees results)
/// - `MaxTokens` → the response was truncated; the agent may continue
///   with a follow-up prompt
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The LLM finished naturally (it has nothing more to say).
    EndTurn,

    /// The LLM stopped because it emitted tool_use blocks that need
    /// to be executed before it can continue.
    ToolUse,

    /// The response was cut off by the `max_tokens` limit.
    MaxTokens,
}

// ---------------------------------------------------------------------------
// StreamEvent
// ---------------------------------------------------------------------------

/// A single event from a streaming LLM response.
///
/// The LLM client produces a `Stream<Item = Result<StreamEvent>>` that the
/// agent's stream handler consumes.  Events arrive in order and represent
/// the incremental construction of the LLM's response.
///
/// ## Event ordering (typical)
///
/// ```text
/// TextDelta("I'll check")
/// TextDelta(" the files.")
/// ToolUseStart { id: "call_1", name: "bash" }
/// ToolUseInputDelta("{\"com")
/// ToolUseInputDelta("mand\":\"ls\"}")
/// ToolUseComplete { id: "call_1", name: "bash", input: {"command":"ls"} }
/// MessageComplete { stop_reason: ToolUse }
/// ```
///
/// Or for a simple text response:
///
/// ```text
/// TextDelta("The answer is 42.")
/// MessageComplete { stop_reason: EndTurn }
/// ```
#[derive(Debug)]
pub enum StreamEvent {
    /// A fragment of text output.
    ///
    /// Printed to the terminal immediately as it arrives.  Multiple
    /// TextDelta events concatenate to form the full text.
    TextDelta(String),

    /// The LLM is starting a tool call.
    ///
    /// At this point we know the tool's `id` and `name`, but the input
    /// JSON hasn't arrived yet (it comes as InputDelta fragments).
    ToolUseStart { id: String, name: String },

    /// A fragment of the tool call's input JSON.
    ///
    /// The Anthropic API streams tool input as partial JSON strings.
    /// The AnthropicClient accumulates these internally and emits
    /// `ToolUseComplete` when the full JSON is ready.  The stream
    /// handler receives these for display/logging purposes only.
    ToolUseInputDelta(String),

    /// A tool call is fully formed and ready for execution.
    ///
    /// `input` is the parsed JSON object.  The stream handler collects
    /// these and returns them to the agent loop for execution.
    ToolUseComplete {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// The LLM has finished generating this message.
    ///
    /// `stop_reason` tells the agent loop whether to execute tools and
    /// continue, or to end the turn.
    MessageComplete { stop_reason: StopReason },

    /// An error occurred during streaming.
    ///
    /// The stream may or may not continue after this.  The stream handler
    /// should surface this to the user and decide whether to abort.
    Error(DysonError),
}
