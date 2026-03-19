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
//   to/from its provider's wire format (see `to_anthropic_value()`).
//
// Why manual serialization instead of serde on Message?
//   The Anthropic API has quirks that make serde cumbersome:
//   - Tool results must be sent as role="user" (not a separate role)
//   - Content blocks have provider-specific shapes
//   - The system prompt is a separate field, not a message
//   Rather than fighting serde with custom serializers and `#[serde(rename)]`
//   gymnastics, we keep `ContentBlock` serde-friendly (useful for logging
//   and debugging) but serialize `Message` to API format explicitly.
//
// Data flow:
//
//   User types text
//     → Message::user("hello")
//     → agent sends to LLM client
//     → LLM client calls msg.to_anthropic_value()  ← provider-specific
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
/// [`Message::to_anthropic_value()`].
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Message {
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

    // -----------------------------------------------------------------------
    // Anthropic API serialization
    // -----------------------------------------------------------------------

    /// Serialize this message to the JSON shape expected by the Anthropic
    /// Messages API.
    ///
    /// ## Why manual serialization?
    ///
    /// The Anthropic API has specific requirements:
    /// - `role` is either `"user"` or `"assistant"` (no "tool" or "system")
    /// - Tool results go in `role: "user"` messages with content blocks
    ///   of `type: "tool_result"`
    /// - Tool use blocks have `type: "tool_use"` with `id`, `name`, `input`
    /// - Text blocks have `type: "text"` with `text`
    ///
    /// Rather than annotating everything with serde renames and custom
    /// serializers, we build the JSON value directly.  This is explicit,
    /// easy to debug, and trivial to adapt when adding new providers.
    pub fn to_anthropic_value(&self) -> serde_json::Value {
        let role_str = match self.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        let content: Vec<serde_json::Value> = self
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => {
                    serde_json::json!({
                        "type": "text",
                        "text": text,
                    })
                }
                ContentBlock::ToolUse { id, name, input } => {
                    serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    })
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                        "is_error": is_error,
                    })
                }
            })
            .collect();

        serde_json::json!({
            "role": role_str,
            "content": content,
        })
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
    fn anthropic_serialization_user() {
        let msg = Message::user("hi");
        let val = msg.to_anthropic_value();
        assert_eq!(val["role"], "user");
        assert_eq!(val["content"][0]["type"], "text");
        assert_eq!(val["content"][0]["text"], "hi");
    }

    #[test]
    fn anthropic_serialization_tool_use() {
        let msg = Message::assistant(vec![ContentBlock::ToolUse {
            id: "id_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "echo test"}),
        }]);
        let val = msg.to_anthropic_value();
        assert_eq!(val["role"], "assistant");
        assert_eq!(val["content"][0]["type"], "tool_use");
        assert_eq!(val["content"][0]["id"], "id_1");
        assert_eq!(val["content"][0]["name"], "bash");
        assert_eq!(val["content"][0]["input"]["command"], "echo test");
    }

    #[test]
    fn anthropic_serialization_tool_result() {
        let msg = Message::tool_result("id_1", "output here", true);
        let val = msg.to_anthropic_value();
        assert_eq!(val["role"], "user");
        assert_eq!(val["content"][0]["type"], "tool_result");
        assert_eq!(val["content"][0]["tool_use_id"], "id_1");
        assert_eq!(val["content"][0]["content"], "output here");
        assert_eq!(val["content"][0]["is_error"], true);
    }
}
