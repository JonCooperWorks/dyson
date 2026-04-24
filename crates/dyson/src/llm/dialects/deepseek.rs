// ===========================================================================
// DeepSeek dialect — echoes `reasoning_content` back to the API.
//
// DeepSeek's "thinking mode" (via OpenRouter or direct) streams a
// `reasoning_content` field alongside regular `content`.  Our parser turns
// those deltas into `ContentBlock::Thinking` blocks on the assistant message.
//
// On the NEXT turn, DeepSeek requires the prior `reasoning_content` to be
// echoed back in the same field on the assistant message — omitting it
// raises:
//
//   "The reasoning_content in the thinking mode must be passed back to the API."
//
// The default OpenAI Chat Completions serializer (`message_to_openai`) drops
// `Thinking` blocks because standard OpenAI has no matching field.  This
// dialect is applied only for DeepSeek models, via `OpenAiCompatClient`.
// ===========================================================================

use crate::message::{ContentBlock, Message, Role};

/// Returns `true` if the model name looks like a DeepSeek variant.
///
/// Matches both direct DeepSeek identifiers (`deepseek-chat`, `deepseek-reasoner`)
/// and OpenRouter slugs (`deepseek/deepseek-chat`, `deepseek/deepseek-r1`, etc.).
pub fn is_deepseek_model(model: &str) -> bool {
    model.to_lowercase().contains("deepseek")
}

/// Walk the serialized messages array and inject `reasoning_content` into
/// assistant messages whose original `Message` carried `Thinking` blocks.
///
/// `originals` and `messages_json` are aligned by index *after* the system
/// message (i.e. `messages_json[0]` is the system message, subsequent
/// entries correspond to `originals[i]`).
pub fn inject_reasoning_content(
    originals: &[Message],
    messages_json: &mut [serde_json::Value],
) {
    let system_offset = 1;
    for (i, msg) in originals.iter().enumerate() {
        if !matches!(msg.role, Role::Assistant) {
            continue;
        }
        let reasoning = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if reasoning.is_empty() {
            continue;
        }
        if let Some(out) = messages_json.get_mut(system_offset + i) {
            out["reasoning_content"] = serde_json::json!(reasoning);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_deepseek_models() {
        assert!(is_deepseek_model("deepseek-chat"));
        assert!(is_deepseek_model("deepseek-reasoner"));
        assert!(is_deepseek_model("deepseek/deepseek-r1"));
        assert!(is_deepseek_model("DeepSeek-V3"));
        assert!(!is_deepseek_model("gpt-4o"));
        assert!(!is_deepseek_model("claude-sonnet-4-6"));
        assert!(!is_deepseek_model("google/gemma-3-27b-it"));
    }

    #[test]
    fn injects_reasoning_into_assistant_messages() {
        let originals = vec![
            Message::user("hi"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "step one, step two".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ]),
        ];
        let mut json = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "answer"}),
        ];

        inject_reasoning_content(&originals, &mut json);

        assert!(json[1].get("reasoning_content").is_none());
        assert_eq!(json[2]["reasoning_content"], "step one, step two");
    }

    #[test]
    fn skips_assistants_without_thinking() {
        let originals = vec![Message::assistant(vec![ContentBlock::Text {
            text: "hi".into(),
        }])];
        let mut json = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];

        inject_reasoning_content(&originals, &mut json);

        assert!(json[1].get("reasoning_content").is_none());
    }

    #[test]
    fn skips_user_messages_even_with_thinking_block() {
        // A user message with a Thinking block would be malformed, but the
        // dialect must not leak reasoning_content onto the user role.
        let originals = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Thinking {
                thinking: "nope".into(),
            }],
        }];
        let mut json = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": ""}),
        ];

        inject_reasoning_content(&originals, &mut json);

        assert!(json[1].get("reasoning_content").is_none());
    }
}
