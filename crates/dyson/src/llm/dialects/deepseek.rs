// ===========================================================================
// DeepSeek dialect — thinking-mode parsing and `reasoning_content` echo.
//
// DeepSeek's "thinking mode" (direct API or via OpenRouter) streams reasoning
// chunks alongside regular `content`.  This dialect handles two ends of
// that round trip:
//
//   1. INBOUND.  OpenRouter normalizes DeepSeek's `reasoning_content` to
//      `delta.reasoning` (string, plus a `reasoning_details` array).  The
//      base OpenAI parser only knows about `reasoning_content`, so we wrap
//      it with `DeepSeekJsonParser` to also capture `delta.reasoning`.
//      Without this the Thinking block never gets built and step 2 has
//      nothing to echo.
//
//   2. OUTBOUND.  On the NEXT turn DeepSeek requires the prior
//      `reasoning_content` echoed back on the assistant message or it
//      returns:
//
//        "The reasoning_content in the thinking mode must be passed back to the API."
//
//      The default OpenAI Chat Completions serializer drops `Thinking`
//      blocks because standard OpenAI has no matching field, so we inject
//      `reasoning_content` after serialization via `inject_reasoning_content`.
//
// Applied only for DeepSeek models, gated by `is_deepseek_model()` in
// `OpenAiCompatClient`.
// ===========================================================================

use crate::error::Result;
use crate::llm::openai::OpenAiJsonParser;
use crate::llm::sse_parser::{SseJsonParser, ToolBufferContext};
use crate::llm::stream::StreamEvent;
use crate::message::{ContentBlock, Message, Role};

/// Returns `true` if the model name looks like a DeepSeek variant.
///
/// Matches both direct DeepSeek identifiers (`deepseek-chat`, `deepseek-reasoner`)
/// and OpenRouter slugs (`deepseek/deepseek-chat`, `deepseek/deepseek-r1`, etc.).
pub fn is_deepseek_model(model: &str) -> bool {
    model.to_lowercase().contains("deepseek")
}

/// Wraps [`OpenAiJsonParser`] and additionally scans `choices[].delta.reasoning`
/// (OpenRouter's normalized reasoning field) for Thinking deltas.
///
/// Delegates all other delta handling (content, tool_calls, finish_reason) to
/// the base parser.  We prefer `reasoning_content` when present (DeepSeek
/// direct) and only fall through to `reasoning` otherwise, to avoid
/// double-emitting when a provider sends both.
pub struct DeepSeekJsonParser {
    inner: OpenAiJsonParser,
}

impl DeepSeekJsonParser {
    pub const fn new() -> Self {
        Self {
            inner: OpenAiJsonParser::new(),
        }
    }
}

impl Default for DeepSeekJsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseJsonParser for DeepSeekJsonParser {
    fn parse_json(
        &mut self,
        json: &serde_json::Value,
        ctx: &mut ToolBufferContext,
    ) -> Vec<Result<StreamEvent>> {
        let mut extra: Vec<Result<StreamEvent>> = Vec::new();

        if let Some(choices) = json["choices"].as_array() {
            for choice in choices {
                let delta = &choice["delta"];
                // Only fill in when the base parser wouldn't already have
                // handled this chunk via `reasoning_content`.
                if delta.get("reasoning_content").is_none()
                    && let Some(text) = delta["reasoning"].as_str()
                    && !text.is_empty()
                {
                    extra.push(Ok(StreamEvent::ThinkingDelta(text.to_string())));
                }
            }
        }

        extra.extend(self.inner.parse_json(json, ctx));
        extra
    }
}

/// Walk the serialized messages array and inject `reasoning_content` into
/// every assistant message.  Messages with `Thinking` blocks get the
/// concatenated reasoning text; messages without get an empty string.
///
/// DeepSeek's thinking mode requires `reasoning_content` on EVERY prior
/// assistant message in the history, not just ones with reasoning — even
/// assistant messages that came from a different (non-thinking) model
/// before the user switched to DeepSeek mid-conversation.  Without this
/// DeepSeek 400s with "The reasoning_content in the thinking mode must
/// be passed back to the API" for model-switch scenarios.
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
        if let Some(out) = messages_json.get_mut(system_offset + i) {
            out["reasoning_content"] = serde_json::json!(reasoning);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::sse_parser::BaseSseParser;
    use crate::llm::SseStreamParser;

    #[test]
    fn parser_emits_thinking_from_openrouter_reasoning_field() {
        // Verified against live OpenRouter SSE for deepseek-v4-pro — chunks
        // carry `delta.reasoning` (string) and `delta.reasoning_details`
        // (array) but no `delta.reasoning_content`.  The base parser misses
        // both; the dialect parser must catch the string form.
        let mut parser = BaseSseParser::new(DeepSeekJsonParser::new());
        let events = parser.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"reasoning\":\"step one\",\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"step one\"}]},\"finish_reason\":null}]}\n\n\
              data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n\
              data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        );

        let thinking: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::ThinkingDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["step one".to_string()]);

        let text: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, vec!["answer".to_string()]);
    }

    #[test]
    fn parser_prefers_reasoning_content_when_both_present() {
        // A provider that sends both `reasoning_content` and `reasoning`
        // (unlikely but defensive) must not emit two ThinkingDeltas.
        let mut parser = BaseSseParser::new(DeepSeekJsonParser::new());
        let events = parser.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"primary\",\"reasoning\":\"duplicate\"},\"finish_reason\":null}]}\n\n"
        );

        let thinking: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::ThinkingDelta(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["primary".to_string()]);
    }

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
    fn injects_empty_reasoning_content_for_assistants_without_thinking() {
        // Covers the model-switch case: history from a non-thinking model
        // (e.g. qwen) flows into a DeepSeek turn.  DeepSeek requires the
        // field present on every assistant message when thinking is active,
        // even if empty.
        let originals = vec![Message::assistant(vec![ContentBlock::Text {
            text: "hi".into(),
        }])];
        let mut json = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];

        inject_reasoning_content(&originals, &mut json);

        assert_eq!(json[1]["reasoning_content"], "");
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
