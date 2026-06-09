// ===========================================================================
// Qwen tool call handling — TextToolHandler for Qwen model family.
//
// Qwen models commonly emit tool calls as XML-ish text:
//
//   <tool_call>
//   <function=write_file>
//   <parameter=file_path>
//   report.md
//   </parameter>
//   <parameter=content>
//   ...
//   </parameter>
//   </function>
//   </tool_call>
//
// Some OpenAI-compatible providers accept a `tools` array for Qwen but the
// model can still fall back to this native text format, especially for large
// file-writing calls. Mixing the two modes leaves half-formed structured tool
// calls and can kill the upstream SSE stream. Route Qwen through the text
// dialect so there is one tool calling protocol in play.
// ===========================================================================

use regex::Regex;
use std::sync::LazyLock;

use super::{ExtractedToolCall, TextToolHandler};
use crate::llm::ToolDefinition;

// ---------------------------------------------------------------------------
// Model detection
// ---------------------------------------------------------------------------

pub fn is_qwen_model(model: &str) -> bool {
    model.to_lowercase().contains("qwen")
}

// ---------------------------------------------------------------------------
// QwenToolHandler
// ---------------------------------------------------------------------------

pub struct QwenToolHandler;

impl TextToolHandler for QwenToolHandler {
    fn format_tools_for_prompt(&self, tools: &[ToolDefinition]) -> String {
        format_tools_for_prompt(tools)
    }

    fn extract_tool_calls(&self, text: &str) -> Option<(String, Vec<ExtractedToolCall>)> {
        extract_qwen_tool_calls(text)
    }
}

// ---------------------------------------------------------------------------
// Tool prompt injection
// ---------------------------------------------------------------------------

fn format_tools_for_prompt(tools: &[ToolDefinition]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut prompt = String::from(
        "\n\n# Available tools\n\n\
         You have access to tools. To call a tool, emit exactly this XML format \
         and no prose in the same message:\n\n\
         <tool_call>\n\
         <function=tool_name>\n\
         <parameter=param_name>\n\
         value\n\
         </parameter>\n\
         </function>\n\
         </tool_call>\n\n\
         Use one <parameter=...> block for each argument. Do not wrap the tool \
         call in Markdown fences. For file writes, keep each content argument \
         concise; prefer a concise file over a huge single tool call.\n\n\
         Tools:\n",
    );

    super::write_tool_param_table(&mut prompt, tools);

    prompt
}

// ---------------------------------------------------------------------------
// Tool call extraction
// ---------------------------------------------------------------------------

fn extract_qwen_tool_calls(text: &str) -> Option<(String, Vec<ExtractedToolCall>)> {
    static TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<tool_call>\s*<function(?:=([^>\s]+)|[^>]*\bname\s*=\s*["']?([^>"'\s]+)["']?[^>]*)>(.*?)</function>\s*</tool_call>"#,
        )
            .expect("qwen tool-call regex")
    });
    static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?s)<parameter(?:=([^>\s]+)|[^>]*\bname\s*=\s*["']?([^>"'\s]+)["']?[^>]*)>(.*?)</parameter>"#,
        )
        .expect("qwen parameter regex")
    });

    if !TOOL_CALL_RE.is_match(text) {
        return None;
    }

    let mut calls = Vec::new();
    for cap in TOOL_CALL_RE.captures_iter(text) {
        let name = cap
            .get(1)
            .or_else(|| cap.get(2))
            .expect("function regex captures a name")
            .as_str()
            .trim()
            .to_string();
        let body = cap.get(3).expect("function regex captures a body").as_str();
        let mut map = serde_json::Map::new();
        for param in PARAM_RE.captures_iter(body) {
            let key = param
                .get(1)
                .or_else(|| param.get(2))
                .expect("parameter regex captures a name")
                .as_str()
                .trim()
                .to_string();
            let value = normalize_param_value(
                param
                    .get(3)
                    .expect("parameter regex captures a value")
                    .as_str(),
            );
            map.insert(key, serde_json::Value::String(value));
        }
        calls.push(ExtractedToolCall {
            name,
            input: serde_json::Value::Object(map),
        });
    }

    if calls.is_empty() {
        return None;
    }

    let cleaned = TOOL_CALL_RE.replace_all(text, "").trim().to_string();
    Some((cleaned, calls))
}

fn normalize_param_value(raw: &str) -> String {
    let mut value = raw;
    if let Some(stripped) = value.strip_prefix("\r\n") {
        value = stripped;
    } else if let Some(stripped) = value.strip_prefix('\n') {
        value = stripped;
    }
    if let Some(stripped) = value.strip_suffix("\r\n") {
        value = stripped;
    } else if let Some(stripped) = value.strip_suffix('\n') {
        value = stripped;
    }
    value.to_string()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qwen_models() {
        assert!(is_qwen_model("qwen/qwen3.6-max-preview"));
        assert!(is_qwen_model("Qwen3.6-Max"));
        assert!(!is_qwen_model("google/gemma-3-27b-it"));
        assert!(!is_qwen_model("deepseek/deepseek-v4-pro"));
    }

    #[test]
    fn extracts_qwen_xml_tool_call() {
        let text = "<tool_call>\n<function=write_file>\n<parameter=file_path>\nreport.md\n</parameter>\n<parameter=content>\nhello\nworld\n</parameter>\n</function>\n</tool_call>";
        let (cleaned, calls) = extract_qwen_tool_calls(text).unwrap();
        assert!(cleaned.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].input["file_path"], "report.md");
        assert_eq!(calls[0].input["content"], "hello\nworld");
    }

    #[test]
    fn extracts_qwen_xml_tool_call_with_name_attributes() {
        let text = "<tool_call><function name=\"write_file\"><parameter name=\"file_path\">report.md</parameter><parameter name='content'>hello</parameter></function></tool_call>";
        let (_, calls) = extract_qwen_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].input["file_path"], "report.md");
        assert_eq!(calls[0].input["content"], "hello");
    }

    #[test]
    fn removes_tool_call_from_surrounding_text() {
        let text = "Preparing file.\n<tool_call>\n<function=send_file>\n<parameter=file_path>\nreport.md\n</parameter>\n</function>\n</tool_call>\nDone.";
        let (cleaned, calls) = extract_qwen_tool_calls(text).unwrap();
        assert_eq!(cleaned, "Preparing file.\n\nDone.");
        assert_eq!(calls[0].name, "send_file");
        assert_eq!(calls[0].input["file_path"], "report.md");
    }

    #[test]
    fn extracts_multiple_calls() {
        let text = "<tool_call><function=write_file><parameter=file_path>a.txt</parameter><parameter=content>a</parameter></function></tool_call>\n<tool_call><function=send_file><parameter=file_path>a.txt</parameter></function></tool_call>";
        let (_, calls) = extract_qwen_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[1].name, "send_file");
    }

    #[test]
    fn no_match_returns_none() {
        assert!(extract_qwen_tool_calls("Just text").is_none());
        assert!(extract_qwen_tool_calls("<function=write_file></function>").is_none());
    }

    #[test]
    fn prompt_mentions_qwen_xml_format() {
        let tools = vec![ToolDefinition {
            name: "write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Path"},
                    "content": {"type": "string", "description": "Content"}
                },
                "required": ["file_path", "content"]
            }),
            agent_only: true,
        }];
        let prompt = format_tools_for_prompt(&tools);
        assert!(prompt.contains("<tool_call>"));
        assert!(prompt.contains("<function=tool_name>"));
        assert!(prompt.contains("file_path: string (required)"));
    }
}
