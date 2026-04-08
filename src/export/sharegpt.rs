// ===========================================================================
// ShareGPT export — convert Dyson conversations to ShareGPT format.
//
// LEARNING OVERVIEW
//
// ShareGPT format is a JSON structure used widely for fine-tuning LLMs:
//
//   {
//     "conversations": [
//       { "from": "system", "value": "You are a helpful assistant." },
//       { "from": "human", "value": "Hello" },
//       { "from": "gpt", "value": "Hi! How can I help?" },
//       { "from": "human", "value": "<tool_call>bash({\"command\": \"ls\"})</tool_call>" },
//       { "from": "tool", "value": "file.txt\nREADME.md" },
//       { "from": "gpt", "value": "Here are the files." }
//     ]
//   }
//
// Dyson's internal format uses `Message` with `ContentBlock` variants
// (Text, ToolUse, ToolResult).  A single Dyson message can contain
// multiple blocks (e.g., text + tool_use).  ShareGPT expects one turn
// per entry, so we flatten multi-block messages into multiple turns.
//
// Tool calls are serialized as:
//   <tool_call>tool_name(json_input)</tool_call>
//
// This is a common convention in training data for tool-calling models
// (used by Hermes, Gorilla, and others).
// ===========================================================================

use serde::Serialize;

use crate::message::{ContentBlock, Message, Role};

// ---------------------------------------------------------------------------
// ShareGPT types
// ---------------------------------------------------------------------------

/// A single conversation in ShareGPT format.
#[derive(Debug, Clone, Serialize)]
pub struct ShareGptConversation {
    /// Unique identifier for this conversation (optional but conventional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The ordered list of turns in this conversation.
    pub conversations: Vec<ShareGptTurn>,
}

/// A single turn in a ShareGPT conversation.
#[derive(Debug, Clone, Serialize)]
pub struct ShareGptTurn {
    /// The speaker: "system", "human", "gpt", or "tool".
    pub from: String,

    /// The content of this turn.
    pub value: String,
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

/// Convert Dyson messages to a ShareGPT conversation.
///
/// Optionally prepends a system turn with the given system prompt.
/// Tool calls are serialized with `<tool_call>` tags.  Tool results
/// become "tool" turns.  Multi-block assistant messages are merged
/// into a single turn with tool calls appended after text.
pub fn to_sharegpt(
    messages: &[Message],
    system_prompt: Option<&str>,
    id: Option<String>,
) -> ShareGptConversation {
    let mut turns = Vec::new();

    // Optionally inject the system prompt as the first turn.
    if let Some(prompt) = system_prompt
        && !prompt.is_empty()
    {
        turns.push(ShareGptTurn {
            from: "system".to_string(),
            value: prompt.to_string(),
        });
    }

    for message in messages {
        // Separate content blocks by type for this message.
        let cap = message.content.len();
        let mut text_parts: Vec<String> = Vec::with_capacity(cap);
        let mut tool_calls: Vec<String> = Vec::with_capacity(cap);
        let mut tool_results: Vec<(String, bool)> = Vec::with_capacity(cap);

        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        text_parts.push(text.clone());
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let input_str =
                        serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(format!("<tool_call>{name}({input_str})</tool_call>"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    tool_results.push((content.clone(), *is_error));
                }
                ContentBlock::Image { media_type, .. } => {
                    text_parts.push(format!("[Image: {media_type}]"));
                }
                ContentBlock::Thinking { .. } => {}
            }
        }

        // Emit tool results as "tool" turns (these come in User-role messages
        // in Dyson's format, but are semantically tool responses).
        if !tool_results.is_empty() {
            for (content, is_error) in &tool_results {
                let value = if *is_error {
                    format!("<tool_error>{content}</tool_error>")
                } else {
                    content.clone()
                };
                turns.push(ShareGptTurn {
                    from: "tool".to_string(),
                    value,
                });
            }
            // If there were also text parts in this message (unlikely for
            // tool result messages, but handle gracefully), emit them too.
            if !text_parts.is_empty() {
                turns.push(ShareGptTurn {
                    from: role_to_sharegpt(&message.role).to_string(),
                    value: text_parts.join("\n"),
                });
            }
            continue;
        }

        // For assistant messages: merge text + tool calls into one turn.
        // For user messages: just emit text.
        match message.role {
            Role::Assistant => {
                let mut value_parts = text_parts;
                value_parts.extend(tool_calls);
                if !value_parts.is_empty() {
                    turns.push(ShareGptTurn {
                        from: "gpt".to_string(),
                        value: value_parts.join("\n"),
                    });
                }
            }
            Role::User => {
                if !text_parts.is_empty() {
                    turns.push(ShareGptTurn {
                        from: "human".to_string(),
                        value: text_parts.join("\n"),
                    });
                }
            }
        }
    }

    ShareGptConversation {
        id,
        conversations: turns,
    }
}

/// Convert a batch of conversations to a ShareGPT JSON array.
///
/// This is the format expected by most fine-tuning frameworks:
/// a JSON array where each element is a conversation object.
pub fn to_sharegpt_json(conversations: &[ShareGptConversation]) -> crate::Result<String> {
    Ok(serde_json::to_string_pretty(conversations)?)
}

fn role_to_sharegpt(role: &Role) -> &'static str {
    match role {
        Role::User => "human",
        Role::Assistant => "gpt",
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn simple_conversation() {
        let messages = vec![
            Message::user("Hello"),
            Message::assistant(vec![ContentBlock::Text {
                text: "Hi! How can I help?".into(),
            }]),
        ];

        let conv = to_sharegpt(&messages, None, None);
        assert_eq!(conv.conversations.len(), 2);
        assert_eq!(conv.conversations[0].from, "human");
        assert_eq!(conv.conversations[0].value, "Hello");
        assert_eq!(conv.conversations[1].from, "gpt");
        assert_eq!(conv.conversations[1].value, "Hi! How can I help?");
    }

    #[test]
    fn with_system_prompt() {
        let messages = vec![Message::user("Hello")];
        let conv = to_sharegpt(&messages, Some("You are helpful."), None);
        assert_eq!(conv.conversations.len(), 2);
        assert_eq!(conv.conversations[0].from, "system");
        assert_eq!(conv.conversations[0].value, "You are helpful.");
        assert_eq!(conv.conversations[1].from, "human");
    }

    #[test]
    fn tool_use_and_result() {
        let messages = vec![
            Message::user("List files"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "Let me check.".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ]),
            Message::tool_result("call_1", "file.txt\nREADME.md", false),
            Message::assistant(vec![ContentBlock::Text {
                text: "Here are the files.".into(),
            }]),
        ];

        let conv = to_sharegpt(&messages, None, None);
        assert_eq!(conv.conversations.len(), 4);

        // Human turn
        assert_eq!(conv.conversations[0].from, "human");

        // Assistant turn with tool call
        assert_eq!(conv.conversations[1].from, "gpt");
        assert!(conv.conversations[1].value.contains("Let me check."));
        assert!(conv.conversations[1].value.contains("<tool_call>bash("));

        // Tool result
        assert_eq!(conv.conversations[2].from, "tool");
        assert_eq!(conv.conversations[2].value, "file.txt\nREADME.md");

        // Final assistant turn
        assert_eq!(conv.conversations[3].from, "gpt");
        assert_eq!(conv.conversations[3].value, "Here are the files.");
    }

    #[test]
    fn tool_error_result() {
        let messages = vec![Message::tool_result("call_1", "command not found", true)];

        let conv = to_sharegpt(&messages, None, None);
        assert_eq!(conv.conversations.len(), 1);
        assert_eq!(conv.conversations[0].from, "tool");
        assert!(conv.conversations[0].value.contains("<tool_error>"));
        assert!(conv.conversations[0].value.contains("command not found"));
    }

    #[test]
    fn empty_messages() {
        let conv = to_sharegpt(&[], None, None);
        assert!(conv.conversations.is_empty());
    }

    #[test]
    fn with_id() {
        let conv = to_sharegpt(&[], None, Some("conv-001".into()));
        assert_eq!(conv.id, Some("conv-001".into()));
    }

    #[test]
    fn batch_json_serialization() {
        let convs = vec![
            to_sharegpt(
                &[
                    Message::user("Hi"),
                    Message::assistant(vec![ContentBlock::Text {
                        text: "Hello!".into(),
                    }]),
                ],
                None,
                Some("conv-1".into()),
            ),
            to_sharegpt(&[Message::user("Bye")], None, Some("conv-2".into())),
        ];

        let json = to_sharegpt_json(&convs).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn multiple_tool_calls_in_one_message() {
        let messages = vec![Message::assistant(vec![
            ContentBlock::Text {
                text: "I'll run two commands.".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            },
            ContentBlock::ToolUse {
                id: "c2".into(),
                name: "bash".into(),
                input: json!({"command": "pwd"}),
            },
        ])];

        let conv = to_sharegpt(&messages, None, None);
        assert_eq!(conv.conversations.len(), 1);
        assert_eq!(conv.conversations[0].from, "gpt");
        // Should have both tool calls
        let value = &conv.conversations[0].value;
        assert!(value.contains("<tool_call>bash("));
        assert_eq!(value.matches("<tool_call>").count(), 2);
    }

    #[test]
    fn empty_system_prompt_skipped() {
        let conv = to_sharegpt(&[Message::user("Hi")], Some(""), None);
        assert_eq!(conv.conversations.len(), 1);
        assert_eq!(conv.conversations[0].from, "human");
    }
}
