// ===========================================================================
// Message types — the lingua franca between agent, LLM, and tools.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the core data types that represent a conversation: `Message`,
//   `Role`, and `ContentBlock`.  Every component in Dyson speaks this
//   language — the agent loop builds messages, the LLM client serializes
//   them for the API, and tool results come back as messages too.
//
// Why not just use the Anthropic API types directly?
//   Dyson is provider-agnostic.  The `LlmClient` trait can be backed by
//   Anthropic, OpenAI, or a local model.  These internal types are the
//   common denominator.  Each LLM client is responsible for converting
//   to/from its provider's wire format (see `message_to_anthropic()` in
//   `llm/anthropic.rs` and `message_to_openai()` in `llm/openai.rs`).
//
// Why manual serialization instead of serde on Message?
//   The Anthropic API has quirks that make serde cumbersome:
//   - Tool results must be sent as role="user" (not a separate role)
//   - Content blocks have provider-specific shapes
//   - The system prompt is a separate field, not a message
//   Rather than fighting serde with custom serializers and `#[serde(rename)]`
//   gymnastics, we keep `ContentBlock` serde-friendly (useful for logging
//   and debugging) but each provider module serializes `Message` to its
//   API format explicitly.
//
// Data flow:
//
//   User types text
//     → Message::user("hello")
//     → agent sends to LLM client
//     → LLM client converts to provider format  (e.g. message_to_anthropic)
//     → HTTP request
//
//   LLM streams back
//     → stream_handler builds ContentBlocks from StreamEvents
//     → Message::assistant(blocks)
//     → appended to conversation history
//
//   Tool executes
//     → ToolOutput { content, is_error }
//     → Message::tool_result(id, content, is_error)
//     → appended to history (serialized as role="user" for Anthropic)
// ===========================================================================

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

/// The speaker of a message in the conversation.
///
/// Note: there is no `System` role.  The system prompt is passed separately
/// to [`LlmClient::stream()`] — it is never part of the message history.
/// There is no `Tool` role either: tool results are sent as `User` messages
/// with `ToolResult` content blocks (this is how the Anthropic API works,
/// and we adopt the same convention internally).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

// ---------------------------------------------------------------------------
// ContentBlock
// ---------------------------------------------------------------------------

/// A single block of content within a message.
///
/// Messages can contain multiple blocks — for example, an assistant message
/// might contain a `Text` block followed by a `ToolUse` block.  This mirrors
/// the Anthropic API's content array structure.
///
/// The `#[serde(tag = "type")]` attribute produces JSON like:
/// ```json
/// { "type": "text", "text": "Hello!" }
/// { "type": "tool_use", "id": "...", "name": "bash", "input": {...} }
/// ```
/// This is handy for debug logging.  For actual API serialization, see
/// `message_to_anthropic()` in `llm/anthropic.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content (user input or LLM output).
    Text {
        text: String,
    },

    /// The LLM is requesting to use a tool.
    ///
    /// `id` is a unique identifier for this specific call — the corresponding
    /// `ToolResult` must reference the same `id` so the LLM can match the
    /// result to its request.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// The result of executing a tool, sent back to the LLM.
    ///
    /// `tool_use_id` must match the `id` from the corresponding `ToolUse`
    /// block.  The Anthropic API rejects messages where a `tool_use` has
    /// no matching `tool_result`.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },

    /// A base64-encoded image for vision-capable models.
    ///
    /// Images are resized and encoded by the media resolver before reaching
    /// the message pipeline.  Each provider serializes this differently:
    /// - Anthropic: `{"type": "image", "source": {"type": "base64", ...}}`
    /// - OpenAI:    `{"type": "image_url", "image_url": {"url": "data:...;base64,..."}}`
    /// - CLI subprocess clients: `[Image attached]` placeholder text
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type, e.g. `"image/jpeg"`, `"image/png"`.
        media_type: String,
    },
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A single message in the conversation history.
///
/// The agent loop maintains a `Vec<Message>` that grows over the
/// conversation.  Each turn appends one or more messages:
///
/// 1. User message (the human's input)
/// 2. Assistant message (LLM's response — may contain text + tool_use blocks)
/// 3. Tool result messages (one per tool call, with role=User)
///
/// Then back to step 2 if the LLM made tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

// ---------------------------------------------------------------------------
// Constructors — ergonomic builders for common message shapes.
// ---------------------------------------------------------------------------

impl ContentBlock {
    /// Rough offline token estimate for this content block.
    ///
    /// Uses whitespace splitting (consistent with `stream_handler.rs`) to
    /// approximate token count without calling a tokenizer.  Good enough
    /// for deciding when to compact — not meant for billing accuracy.
    pub fn estimate_tokens(&self) -> usize {
        match self {
            ContentBlock::Text { text } => text.split_whitespace().count().max(1),
            ContentBlock::ToolUse { id, name, input } => {
                let input_str = input.to_string();
                name.split_whitespace().count()
                    + id.split_whitespace().count()
                    + input_str.split_whitespace().count()
                    + 10 // JSON structure overhead
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                tool_use_id.split_whitespace().count()
                    + content.split_whitespace().count()
                    + 5 // JSON structure overhead
            }
            ContentBlock::Image { data, .. } => {
                // Anthropic charges ~1600 tokens for a 1568x1568 image.
                // Rough heuristic based on base64 data size.
                let decoded_bytes = data.len() * 3 / 4;
                (decoded_bytes / 750).max(100)
            }
        }
    }
}

impl Message {
    /// Rough offline token estimate for this entire message.
    pub fn estimate_tokens(&self) -> usize {
        let content_tokens: usize = self.content.iter().map(|b| b.estimate_tokens()).sum();
        content_tokens + 4 // role + message framing overhead
    }

    /// Create a user message from plain text.
    ///
    /// ```ignore
    /// let msg = Message::user("What files are in the current directory?");
    /// ```
    pub fn user(text: &str) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    /// Create an assistant message from accumulated content blocks.
    ///
    /// Called by the stream handler after processing all `StreamEvent`s for
    /// a single LLM turn.  The blocks typically include `Text` and
    /// optionally `ToolUse` blocks.
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    /// Create a user message from multiple content blocks (text + images).
    ///
    /// Used by the Telegram controller when a message contains media.
    /// ```ignore
    /// let msg = Message::user_multimodal(vec![
    ///     ContentBlock::Text { text: "What's in this image?".into() },
    ///     ContentBlock::Image { data: base64_data, media_type: "image/jpeg".into() },
    /// ]);
    /// ```
    pub fn user_multimodal(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::User,
            content,
        }
    }

    /// Create a tool result message.
    ///
    /// Although this conceptually "belongs" to the tool, it is sent as a
    /// `User` role message because that's what the Anthropic API expects.
    /// The `ToolResult` content block carries the `tool_use_id` to link
    /// it back to the original `ToolUse`.
    pub fn tool_result(tool_use_id: &str, content: &str, is_error: bool) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error,
            }],
        }
    }

}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_shape() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn assistant_message_with_tool_use() {
        let blocks = vec![
            ContentBlock::Text {
                text: "Let me check.".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ];
        let msg = Message::assistant(blocks);
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn tool_result_uses_user_role() {
        let msg = Message::tool_result("call_1", "file.txt\n", false);
        // Tool results are sent as User role — this is critical for the
        // Anthropic API to accept them.
        assert_eq!(msg.role, Role::User);
        match &msg.content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file.txt\n");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got: {other:?}"),
        }
    }

    #[test]
    fn estimate_tokens_text_block() {
        let block = ContentBlock::Text {
            text: "hello world foo bar baz".into(),
        };
        assert_eq!(block.estimate_tokens(), 5);
    }

    #[test]
    fn estimate_tokens_empty_text_returns_at_least_one() {
        let block = ContentBlock::Text {
            text: String::new(),
        };
        assert_eq!(block.estimate_tokens(), 1);
    }

    #[test]
    fn estimate_tokens_tool_use_block() {
        let block = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls -la"}),
        };
        let tokens = block.estimate_tokens();
        // Should include name, id, JSON input, plus overhead.
        assert!(tokens >= 10, "expected at least 10, got {tokens}");
    }

    #[test]
    fn estimate_tokens_tool_result_block() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "file.txt\nREADME.md\n".into(),
            is_error: false,
        };
        let tokens = block.estimate_tokens();
        assert!(tokens >= 5, "expected at least 5, got {tokens}");
    }

    #[test]
    fn estimate_tokens_message_sums_blocks() {
        let msg = Message::assistant(vec![
            ContentBlock::Text {
                text: "one two three".into(),
            },
            ContentBlock::Text {
                text: "four five".into(),
            },
        ]);
        // 3 + 2 words + 4 overhead = 9
        assert_eq!(msg.estimate_tokens(), 9);
    }

}
