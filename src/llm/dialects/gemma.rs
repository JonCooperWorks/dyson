// ===========================================================================
// Gemma tool call handling — TextToolHandler for Gemma model family.
//
// Gemma models emit tool calls as plain text rather than structured
// `tool_calls` arrays.  This module implements `TextToolHandler` to:
//   1. Inject tool definitions into the system prompt
//   2. Parse `call:name{params}` from the model's text output
//
// Supported output formats:
//   - FunctionGemma tags:
//       <start_function_call>call:name{key: value}<end_function_call>
//   - Bare call syntax:
//       call:name{key: value}
//
// Parameter values may be bare, single-quoted, or double-quoted.
// ===========================================================================

use regex::Regex;
use std::sync::LazyLock;

use super::{ExtractedToolCall, TextToolHandler};
use crate::llm::ToolDefinition;

// ---------------------------------------------------------------------------
// Model detection
// ---------------------------------------------------------------------------

/// Returns `true` if the model name looks like a Gemma variant.
pub fn is_gemma_model(model: &str) -> bool {
    model.to_lowercase().contains("gemma")
}

// ---------------------------------------------------------------------------
// GemmaToolHandler
// ---------------------------------------------------------------------------

/// [`TextToolHandler`] implementation for Google Gemma models.
pub struct GemmaToolHandler;

impl TextToolHandler for GemmaToolHandler {
    fn format_tools_for_prompt(&self, tools: &[ToolDefinition]) -> String {
        format_tools_for_prompt(tools)
    }

    fn extract_tool_calls(&self, text: &str) -> Option<(String, Vec<ExtractedToolCall>)> {
        extract_gemma_tool_calls(text)
    }
}

// ---------------------------------------------------------------------------
// Tool prompt injection
// ---------------------------------------------------------------------------

/// Build a system prompt suffix that describes available tools in the
/// format Gemma expects for function calling.
fn format_tools_for_prompt(tools: &[ToolDefinition]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut prompt = String::from(
        "\n\n# Available tools\n\n\
         You have access to the following tools. To use a tool, output a \
         function call using this exact syntax (one per line):\n\n\
         call:tool_name{param1: 'value1', param2: 'value2'}\n\n\
         Always use the call: syntax to invoke tools. Do not describe \
         the tool call in prose — just emit the call: line.\n\n\
         Tools:\n",
    );

    for tool in tools {
        prompt.push_str(&format!("\n## {}\n", tool.name));
        prompt.push_str(&format!("{}\n", tool.description));

        if let Some(props) = tool.input_schema.get("properties")
            && let Some(obj) = props.as_object()
        {
            let required: Vec<&str> = tool
                .input_schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            prompt.push_str("Parameters:\n");
            for (name, schema) in obj {
                let typ = schema
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("string");
                let desc = schema
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let req = if required.contains(&name.as_str()) {
                    " (required)"
                } else {
                    " (optional)"
                };
                prompt.push_str(&format!("  - {name}: {typ}{req} — {desc}\n"));
            }
        }
    }

    prompt
}

// ---------------------------------------------------------------------------
// Tool call extraction
// ---------------------------------------------------------------------------

/// Scan `text` for Gemma-style tool calls.
///
/// Returns `None` if no tool calls are found.  Otherwise returns the
/// cleaned text (tool call portions removed) and the extracted calls.
fn extract_gemma_tool_calls(text: &str) -> Option<(String, Vec<ExtractedToolCall>)> {
    let mut calls = Vec::new();

    // Pattern 1: <start_function_call>call:name{...}<end_function_call>
    static TAGGED_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"<start_function_call>\s*call:(\w+)\{([^}]*)\}\s*<end_function_call>")
            // INVARIANT: hardcoded regex literal — compile failure is a code bug.
            .expect("tagged gemma regex")
    });

    // Pattern 2: bare call:name{...}
    static BARE_RE: LazyLock<Regex> = LazyLock::new(|| {
        // INVARIANT: hardcoded regex literal — compile failure is a code bug.
        Regex::new(r"call:(\w+)\{([^}]*)\}").expect("bare gemma regex")
    });

    let re = if TAGGED_RE.is_match(text) {
        &*TAGGED_RE
    } else if BARE_RE.is_match(text) {
        &*BARE_RE
    } else {
        return None;
    };

    for cap in re.captures_iter(text) {
        let name = cap[1].to_string();
        let params_str = &cap[2];
        let input = parse_gemma_params(params_str);
        calls.push(ExtractedToolCall { name, input });
    }

    if calls.is_empty() {
        return None;
    }

    // replace_all returns Cow — avoids allocation when there are no matches
    // (already handled above), and allocates only once when there are.
    let cleaned = re.replace_all(text, "").trim().to_string();

    Some((cleaned, calls))
}

// ---------------------------------------------------------------------------
// Parameter parsing
// ---------------------------------------------------------------------------

/// Parse Gemma's key-value parameter format into a JSON object.
///
/// Handles:
///   - `key: 'value'`  (single-quoted)
///   - `key: "value"`  (double-quoted)
///   - `key: value`    (bare — terminated by `,` or end of string)
///   - JSON: `"key": "value"` (tried first)
///   - FunctionGemma `<escape>value<escape>` wrapper
fn parse_gemma_params(s: &str) -> serde_json::Value {
    // Try parsing as JSON directly — some Gemma variants emit JSON.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&format!("{{{s}}}"))
        && v.is_object()
    {
        return v;
    }

    let mut map = serde_json::Map::new();
    let s = s.trim();
    if s.is_empty() {
        return serde_json::Value::Object(map);
    }

    let mut remaining = s;

    while !remaining.is_empty() {
        remaining = remaining.trim_start_matches([',', ' ', '\n', '\t']);
        if remaining.is_empty() {
            break;
        }

        let Some(colon_pos) = remaining.find(':') else {
            break;
        };
        let key = remaining[..colon_pos].trim().to_string();
        remaining = remaining[colon_pos + 1..].trim_start();

        if remaining.is_empty() {
            map.insert(key, serde_json::Value::String(String::new()));
            break;
        }

        let (value, rest) = extract_value(remaining);
        map.insert(key, serde_json::Value::String(value));
        remaining = rest;
    }

    serde_json::Value::Object(map)
}

/// Extract a single value from the parameter string.
fn extract_value(s: &str) -> (String, &str) {
    let s = s.trim_start();

    // FunctionGemma <escape> wrappers.
    if let Some(inner) = s.strip_prefix("<escape>")
        && let Some(end) = inner.find("<escape>")
    {
        let value = inner[..end].to_string();
        let rest = &inner[end + "<escape>".len()..];
        return (value, rest);
    }

    // Single-quoted.
    if let Some(inner) = s.strip_prefix('\'')
        && let Some(end) = inner.find('\'')
    {
        let value = inner[..end].to_string();
        let rest = &inner[1 + end..];
        return (value, rest);
    }

    // Double-quoted.
    if let Some(inner) = s.strip_prefix('"')
        && let Some(end) = inner.find('"')
    {
        let value = inner[..end].to_string();
        let rest = &inner[1 + end..];
        return (value, rest);
    }

    // Bare value — terminated by comma or end of string.
    if let Some(comma) = s.find(',') {
        let value = s[..comma].trim().to_string();
        (value, &s[comma..])
    } else {
        (s.trim().to_string(), "")
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Extraction tests --

    #[test]
    fn bare_call_single_param() {
        let text = "call:bash{command: 'tailscale ip -4'}";
        let (cleaned, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input["command"], "tailscale ip -4");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn bare_call_multiple_params() {
        let text = "call:write_file{path: '/tmp/test.txt', content: 'hello world'}";
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].input["path"], "/tmp/test.txt");
        assert_eq!(calls[0].input["content"], "hello world");
    }

    #[test]
    fn tagged_format() {
        let text = "<start_function_call>call:bash{command: 'ls -la'}<end_function_call>";
        let (cleaned, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input["command"], "ls -la");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn escape_tagged_values() {
        let text = "call:bash{command: <escape>echo 'hello'<escape>}";
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls[0].input["command"], "echo 'hello'");
    }

    #[test]
    fn text_before_and_after_call() {
        let text = "Let me check that.\ncall:bash{command: 'ls'}\nDone.";
        let (cleaned, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(cleaned, "Let me check that.\n\nDone.");
    }

    #[test]
    fn multiple_calls() {
        let text = "call:bash{command: 'ls'}\ncall:bash{command: 'pwd'}";
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].input["command"], "ls");
        assert_eq!(calls[1].input["command"], "pwd");
    }

    #[test]
    fn no_match_returns_none() {
        assert!(extract_gemma_tool_calls("Just normal text").is_none());
        assert!(extract_gemma_tool_calls("").is_none());
        assert!(extract_gemma_tool_calls("call without colon").is_none());
    }

    #[test]
    fn json_style_params() {
        let text = r#"call:bash{"command": "ls -la"}"#;
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn bare_unquoted_value() {
        let text = "call:bash{command: ls}";
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls[0].input["command"], "ls");
    }

    #[test]
    fn double_quoted_value() {
        let text = r#"call:bash{command: "echo hello"}"#;
        let (_, calls) = extract_gemma_tool_calls(text).unwrap();
        assert_eq!(calls[0].input["command"], "echo hello");
    }

    // -- Model detection --

    #[test]
    fn detects_gemma_models() {
        assert!(is_gemma_model("google/gemma-3-27b-it"));
        assert!(is_gemma_model("google/gemma-2-9b-it"));
        assert!(is_gemma_model("Gemma-3-27B"));
        assert!(!is_gemma_model("gpt-4o"));
        assert!(!is_gemma_model("claude-sonnet-4-20250514"));
        assert!(!is_gemma_model("meta-llama/llama-3-70b"));
    }

    // -- Prompt formatting --

    #[test]
    fn format_tools_empty() {
        assert_eq!(format_tools_for_prompt(&[]), "");
    }

    #[test]
    fn format_tools_includes_name_and_description() {
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute"
                    }
                },
                "required": ["command"]
            }),
            agent_only: false,
        }];
        let prompt = format_tools_for_prompt(&tools);
        assert!(prompt.contains("## bash"));
        assert!(prompt.contains("Run a shell command"));
        assert!(prompt.contains("command: string (required)"));
        assert!(prompt.contains("call:tool_name"));
    }

    // -- Trait implementation --

    #[test]
    fn trait_extract_delegates_to_parser() {
        let handler = GemmaToolHandler;
        let result = handler.extract_tool_calls("call:bash{command: 'ls'}");
        assert!(result.is_some());
        let (_, calls) = result.unwrap();
        assert_eq!(calls[0].name, "bash");
    }

    #[test]
    fn trait_format_delegates_to_formatter() {
        let handler = GemmaToolHandler;
        let tools = vec![ToolDefinition {
            name: "test".into(),
            description: "A test tool".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            agent_only: false,
        }];
        let prompt = handler.format_tools_for_prompt(&tools);
        assert!(prompt.contains("## test"));
    }
}
