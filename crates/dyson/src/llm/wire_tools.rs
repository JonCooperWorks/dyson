// ===========================================================================
// Wire tools — fit Dyson's tool list into the OpenAI function-calling limits.
//
// OpenAI's Chat Completions API refuses a request (HTTP 400, no failover)
// whose `tools` break either of two rules:
//
//   - a function name must match `^[a-zA-Z0-9_-]{1,64}$`;
//   - at most 128 functions per request.
//
// Dyson breaks both as soon as an agent has a few MCP servers attached: MCP
// servers name tools freely (Swarm's are `artefacts.search`, `a2a.send`),
// and six servers easily bring an agent past 128 tools. OpenRouter-served
// models tolerate both, so the failure only appears once a model is routed
// to OpenAI itself.
//
// This module translates at the wire, both ways, so nothing else in Dyson
// changes:
//
//   - every name is made valid (invalid characters become `_`, a long name
//     is cut and given a short hash), unique, and mapped back on the way in;
//   - past the limit, the tail of the list is offered through ONE dispatcher
//     tool, `more_tools`, whose description lists each tool it carries and
//     whose input is `{"tool": <name>, "arguments": {...}}`. A call to it is
//     handed to the agent as a call to the named tool, and history is
//     re-encoded the same way, so the model always sees one consistent form.
// ===========================================================================

use std::collections::{HashMap, HashSet};

use super::ToolDefinition;
use super::stream::StreamEvent;

/// OpenAI's limit on functions per request.
const MAX_TOOLS: usize = 128;
/// OpenAI's limit on a function name's length.
const MAX_NAME: usize = 64;
/// The dispatcher's name. Chosen to be valid and unlikely to collide; if a
/// real tool already uses it, a suffix is added.
const DISPATCHER: &str = "more_tools";
/// Per-tool description budget inside the dispatcher's description.
const DISPATCHED_DESC_CHARS: usize = 160;

/// Whether `name` is a valid OpenAI function name.
fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// FNV-1a, for a short stable suffix; no dependency for eight hex digits.
fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// A valid name for `name`: invalid characters become `_`, and a name that
/// is still too long is cut and given a hash of the original.
fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("tool");
    }
    if out.len() > MAX_NAME {
        out.truncate(MAX_NAME - 9);
        out.push('_');
        out.push_str(&short_hash(name));
    }
    out
}

/// The translation for one request. Built from the tools Dyson offers.
pub(crate) struct WireTools {
    /// Tools sent with their own function entry: (wire name, definition).
    direct: Vec<(String, ToolDefinition)>,
    /// Tools reached through the dispatcher, in order.
    dispatched: Vec<ToolDefinition>,
    /// The dispatcher's wire name, when there is one.
    dispatcher: Option<String>,
    /// Real name → wire name, for direct tools.
    to_wire: HashMap<String, String>,
    /// Wire name → real name, for direct tools.
    from_wire: HashMap<String, String>,
    /// Real names reached through the dispatcher.
    dispatched_names: HashSet<String>,
}

impl WireTools {
    pub(crate) fn new(tools: &[ToolDefinition]) -> Self {
        let over = tools.len() > MAX_TOOLS;
        // One slot is the dispatcher's.
        let direct_count = if over { MAX_TOOLS - 1 } else { tools.len() };

        let mut used: HashSet<String> = HashSet::new();
        let dispatcher = over.then(|| {
            let mut name = DISPATCHER.to_owned();
            let mut n = 2;
            while tools.iter().any(|t| t.name == name) {
                name = format!("{DISPATCHER}_{n}");
                n += 1;
            }
            used.insert(name.clone());
            name
        });

        let mut direct = Vec::with_capacity(direct_count);
        let mut to_wire = HashMap::new();
        let mut from_wire = HashMap::new();
        // Valid names first, so a sanitised name never takes a real one.
        let (head, tail) = tools.split_at(direct_count);
        for tool in head.iter().filter(|t| is_valid(&t.name)) {
            used.insert(tool.name.clone());
        }
        for tool in head {
            let wire = if is_valid(&tool.name) {
                tool.name.clone()
            } else {
                let base = sanitize(&tool.name);
                let mut wire = base.clone();
                let mut n = 2;
                while used.contains(&wire) {
                    let suffix = format!("_{n}");
                    let keep = base.len().min(MAX_NAME - suffix.len());
                    wire = format!("{}{suffix}", &base[..keep]);
                    n += 1;
                }
                used.insert(wire.clone());
                wire
            };
            to_wire.insert(tool.name.clone(), wire.clone());
            from_wire.insert(wire.clone(), tool.name.clone());
            direct.push((wire, tool.clone()));
        }

        let dispatched: Vec<ToolDefinition> = tail.to_vec();
        let dispatched_names = dispatched.iter().map(|t| t.name.clone()).collect();
        if over {
            tracing::info!(
                tools = tools.len(),
                dispatched = dispatched.len(),
                "tool list over the OpenAI limit; the rest go through {DISPATCHER}"
            );
        }
        Self {
            direct,
            dispatched,
            dispatcher,
            to_wire,
            from_wire,
            dispatched_names,
        }
    }

    /// Nothing to translate: every name valid and within the limit.
    pub(crate) fn is_identity(&self) -> bool {
        self.dispatcher.is_none() && self.direct.iter().all(|(wire, t)| *wire == t.name)
    }

    /// The `tools` array for the request body.
    pub(crate) fn tools_json(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = self
            .direct
            .iter()
            .map(|(wire, t)| function(wire, &t.description, &t.input_schema))
            .collect();
        if let Some(name) = &self.dispatcher {
            out.push(function(
                name,
                &self.dispatcher_description(),
                &serde_json::json!({
                    "type": "object",
                    "properties": {
                        "tool": {
                            "type": "string",
                            "description": "Exact name of the tool to call, from the list in this tool's description.",
                            "enum": self.dispatched.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                        },
                        "arguments": {
                            "type": "object",
                            "description": "That tool's arguments, as its parameters describe.",
                        },
                    },
                    "required": ["tool", "arguments"],
                }),
            ));
        }
        out
    }

    fn dispatcher_description(&self) -> String {
        let mut text = String::from(
            "Calls one of the tools below, which did not fit in the tool list. \
             Pass the tool's exact name as `tool` and its arguments as `arguments`.\n",
        );
        for tool in &self.dispatched {
            let desc: String = tool
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(DISPATCHED_DESC_CHARS)
                .collect();
            let params = tool
                .input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            text.push_str(&format!("- {}({params}): {desc}\n", tool.name));
        }
        text
    }

    /// Re-encode past tool calls in an OpenAI `messages` array so history
    /// uses the same names the model is offered now.
    pub(crate) fn encode_history(&self, messages: &mut [serde_json::Value]) {
        for message in messages {
            let Some(calls) = message
                .get_mut("tool_calls")
                .and_then(serde_json::Value::as_array_mut)
            else {
                continue;
            };
            for call in calls {
                let Some(function) = call.get_mut("function") else {
                    continue;
                };
                let Some(name) = function
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if let Some(wire) = self.to_wire.get(&name) {
                    function["name"] = serde_json::Value::String(wire.clone());
                } else if let (true, Some(dispatcher)) =
                    (self.dispatched_names.contains(&name), &self.dispatcher)
                {
                    let arguments = function
                        .get("arguments")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    function["name"] = serde_json::Value::String(dispatcher.clone());
                    function["arguments"] = serde_json::Value::String(
                        serde_json::json!({ "tool": name, "arguments": arguments }).to_string(),
                    );
                } else if !is_valid(&name) {
                    // A tool used earlier that is no longer offered: still
                    // send a valid name, or the whole request is refused.
                    function["name"] = serde_json::Value::String(sanitize(&name));
                }
            }
        }
    }

    /// Map one streamed event back to Dyson's own tool names.
    pub(crate) fn decode(&self, event: StreamEvent) -> StreamEvent {
        match event {
            StreamEvent::ToolUseStart { id, name } => StreamEvent::ToolUseStart {
                id,
                name: self.from_wire.get(&name).cloned().unwrap_or(name),
            },
            StreamEvent::ToolUseComplete { id, name, input } => {
                if self.dispatcher.as_deref() == Some(name.as_str()) {
                    let tool = input
                        .get("tool")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let arguments = input
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    // An argument string the model double-encoded.
                    let arguments = match arguments {
                        serde_json::Value::String(s) => {
                            serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                        }
                        other => other,
                    };
                    match tool {
                        // Unknown names pass through and fail as an unknown
                        // tool, which the model is told and can correct.
                        Some(tool) => StreamEvent::ToolUseComplete {
                            id,
                            name: tool,
                            input: arguments,
                        },
                        None => StreamEvent::ToolUseComplete { id, name, input },
                    }
                } else {
                    StreamEvent::ToolUseComplete {
                        id,
                        name: self.from_wire.get(&name).cloned().unwrap_or(name),
                        input,
                    }
                }
            }
            other => other,
        }
    }
}

fn function(name: &str, description: &str, schema: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": schema,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_owned(),
            description: format!("does {name}"),
            input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            agent_only: false,
        }
    }

    fn wire_names(wire: &WireTools) -> Vec<String> {
        wire.tools_json()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn valid_lists_pass_through_untouched() {
        let wire = WireTools::new(&[tool("bash"), tool("read_file")]);
        assert!(wire.is_identity());
        assert_eq!(wire_names(&wire), ["bash", "read_file"]);
    }

    /// Swarm's MCP tools are named `artefacts.search`; OpenAI 400s on a dot.
    #[test]
    fn dotted_names_are_sent_valid_and_mapped_back() {
        let wire = WireTools::new(&[tool("artefacts.search"), tool("artefacts_search")]);
        let names = wire_names(&wire);
        assert!(names.iter().all(|n| is_valid(n)), "{names:?}");
        // The real `artefacts_search` keeps its name; the dotted one moves.
        assert_eq!(names[1], "artefacts_search");
        assert_ne!(names[0], names[1]);

        let back = wire.decode(StreamEvent::ToolUseComplete {
            id: "c1".into(),
            name: names[0].clone(),
            input: json!({"q": "x"}),
        });
        match back {
            StreamEvent::ToolUseComplete { name, input, .. } => {
                assert_eq!(name, "artefacts.search");
                assert_eq!(input, json!({"q": "x"}));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn long_names_are_cut_with_a_hash() {
        let long = "x".repeat(80);
        let wire = WireTools::new(&[tool(&long)]);
        let names = wire_names(&wire);
        assert!(is_valid(&names[0]));
        assert_eq!(names[0].len(), MAX_NAME);
    }

    /// 141 tools (TARS: 33 built in + 108 MCP) is past OpenAI's 128.
    #[test]
    fn past_the_limit_the_tail_goes_through_one_dispatcher() {
        let tools: Vec<_> = (0..141).map(|i| tool(&format!("t{i}"))).collect();
        let wire = WireTools::new(&tools);
        let names = wire_names(&wire);
        assert_eq!(names.len(), MAX_TOOLS);
        assert_eq!(names.last().map(String::as_str), Some(DISPATCHER));
        let desc = wire.tools_json()[MAX_TOOLS - 1]["function"]["description"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(desc.contains("- t140(q): does t140"));
        assert!(!desc.contains("- t126("));

        let back = wire.decode(StreamEvent::ToolUseComplete {
            id: "c1".into(),
            name: DISPATCHER.into(),
            input: json!({"tool": "t140", "arguments": {"q": "x"}}),
        });
        match back {
            StreamEvent::ToolUseComplete { name, input, .. } => {
                assert_eq!(name, "t140");
                assert_eq!(input, json!({"q": "x"}));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn history_uses_the_names_offered_now() {
        let mut tools: Vec<_> = (0..140).map(|i| tool(&format!("t{i}"))).collect();
        tools[0] = tool("a2a.send");
        let wire = WireTools::new(&tools);
        let mut messages = vec![json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "a2a.send", "arguments": "{\"q\":\"a\"}"}},
                {"id": "2", "type": "function", "function": {"name": "t139", "arguments": "{\"q\":\"b\"}"}},
                {"id": "3", "type": "function", "function": {"name": "gone.tool", "arguments": "{}"}},
            ],
        })];
        wire.encode_history(&mut messages);
        let calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["function"]["name"], "a2a_send");
        assert_eq!(calls[1]["function"]["name"], DISPATCHER);
        let args: serde_json::Value =
            serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"tool": "t139", "arguments": {"q": "b"}}));
        assert_eq!(calls[2]["function"]["name"], "gone_tool");
    }

    #[test]
    fn a_double_encoded_argument_string_is_unwrapped() {
        let tools: Vec<_> = (0..130).map(|i| tool(&format!("t{i}"))).collect();
        let wire = WireTools::new(&tools);
        let back = wire.decode(StreamEvent::ToolUseComplete {
            id: "c1".into(),
            name: DISPATCHER.into(),
            input: json!({"tool": "t129", "arguments": "{\"q\":\"x\"}"}),
        });
        match back {
            StreamEvent::ToolUseComplete { input, .. } => assert_eq!(input, json!({"q": "x"})),
            other => panic!("{other:?}"),
        }
    }
}
