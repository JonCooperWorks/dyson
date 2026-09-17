//! Deserialization types for dyson.json.
use crate::secret::SecretValue;
use serde::Deserialize;

/// Root of the dyson.json file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonRoot {
    /// Schema version — consumed by `migrate()`, retained here so
    /// `deny_unknown_fields` doesn't reject it.
    #[serde(default)]
    #[expect(dead_code, reason = "serde-only compatibility field")]
    pub(super) config_version: Option<u32>,
    /// Named provider configurations.
    ///
    /// ```json
    /// "providers": {
    ///   "claude": { "type": "anthropic", "models": ["claude-sonnet-4-20250514"], "api_key": "..." },
    ///   "gpt":    { "type": "openai",    "models": ["gpt-4o"] }
    /// }
    /// ```
    pub(super) providers: Option<std::collections::HashMap<String, JsonProviderConfig>>,
    pub(super) agent: Option<JsonAgent>,
    pub(super) skills: Option<JsonSkills>,
    pub(super) controllers: Option<Vec<serde_json::Value>>,
    pub(super) sandbox: Option<JsonSandbox>,
    pub(super) workspace: Option<JsonWorkspace>,
    pub(super) chat_history: Option<JsonChatHistory>,
    /// MCP servers — each becomes a Skill that provides tools.
    ///
    /// ```json
    /// "mcp_servers": {
    ///   "github": { "command": "npx", "args": [...], "env": {...} },
    ///   "postgres": { "command": "npx", "args": [...] }
    /// }
    /// ```
    pub(super) mcp_servers: Option<serde_json::Value>,
    /// Audio transcriber configuration.
    pub(super) transcriber: Option<JsonTranscriber>,
    /// Web search provider configuration.
    pub(super) web_search: Option<JsonWebSearch>,
}

/// The `"transcriber"` object.
///
/// ```json
/// "transcriber": {
///   "provider": "whisper-cli",
///   "model": "small"
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonTranscriber {
    /// Transcriber provider: "whisper-cli" (default).
    pub(super) provider: Option<String>,
    /// Model name/size for the provider.
    pub(super) model: Option<String>,
}

/// The `"web_search"` object.
///
/// ```json
/// "web_search": {
///   "provider": "brave",
///   "api_key": { "resolver": "insecure_env", "name": "BRAVE_API_KEY" }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonWebSearch {
    /// Search provider: "brave" (default).
    pub(super) provider: Option<String>,
    /// API key for the search provider.  Supports secret resolution.
    pub(super) api_key: Option<SecretValue>,
    /// Optional base URL override (e.g. for self-hosted SearXNG).
    pub(super) base_url: Option<String>,
}

/// A single provider entry in the `"providers"` map.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonProviderConfig {
    /// Provider type: "anthropic", "openai", "claude-code", "codex".
    #[serde(rename = "type")]
    pub(super) provider_type: String,
    /// Available models for this provider.
    #[serde(default)]
    pub(super) models: Vec<String>,
    /// API key — literal string or secret resolver reference.
    pub(super) api_key: Option<SecretValue>,
    /// Base URL override.
    pub(super) base_url: Option<String>,
}

/// The `"agent"` object.
///
/// Provider-specific fields (api_key, base_url) live in the `"providers"`
/// map.  The agent references a provider by name.  `model` can optionally
/// override the provider's model.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonAgent {
    pub(super) task_budget: Option<crate::agent::task::budget::Limits>,
    /// Optional model override — takes precedence over the provider's model.
    pub(super) model: Option<String>,
    pub(super) max_iterations: Option<usize>,
    pub(super) max_retries: Option<usize>,
    pub(super) max_concurrent_llm_calls: Option<usize>,
    pub(super) max_tokens: Option<u32>,
    pub(super) system_prompt: Option<String>,
    /// Name of the provider from the `"providers"` map.
    pub(super) provider: Option<String>,
    /// Advisor model for the advisor pattern.
    pub(super) smartest_model: Option<String>,
    /// Name of a provider from the `"providers"` map to use for image generation.
    pub(super) image_generation_provider: Option<String>,
    /// Model override for image generation (e.g. "gemini-3-pro-image-preview").
    pub(super) image_generation_model: Option<String>,
    /// Context compaction configuration.  Accepts either:
    /// - an integer: shorthand for `{ "context_window": <value> }` with defaults
    /// - an object: full `CompactionConfig` with optional fields
    pub(super) compaction: Option<JsonCompaction>,
    /// Rate limiting: `{ "max_messages": 30, "window_secs": 60 }`.
    pub(super) rate_limit: Option<JsonRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonRateLimit {
    pub(super) max_messages: usize,
    pub(super) window_secs: u64,
}

/// Flexible deserialization for the `"compaction"` field.
///
/// Accepts either a bare integer (context window shorthand) or a full object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum JsonCompaction {
    /// Shorthand: just the context_window size, e.g. `"compaction": 200000`.
    Window(usize),
    /// Full config object with optional fields.
    Full {
        context_window: Option<usize>,
        threshold_ratio: Option<f64>,
        protect_head: Option<usize>,
        protect_tail_tokens: Option<usize>,
        summary_min_tokens: Option<usize>,
        summary_max_tokens: Option<usize>,
        summary_target_ratio: Option<f64>,
    },
}

/// The `"skills"` object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonSkills {
    pub(super) builtin: Option<JsonBuiltinSkill>,
    pub(super) local: Option<Vec<JsonLocalSkill>>,
    /// Subagent definitions — child agents spawnable as tools.
    ///
    /// ```json
    /// "subagents": [
    ///   {
    ///     "name": "research_agent",
    ///     "description": "Research specialist",
    ///     "system_prompt": "You are a research specialist.",
    ///     "provider": "gpt",
    ///     "max_iterations": 15,
    ///     "tools": ["bash", "web_search"]
    ///   }
    /// ]
    /// ```
    pub(super) subagents: Option<Vec<JsonSubagent>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonLocalSkill {
    pub(super) name: String,
    pub(super) path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonBuiltinSkill {
    pub(super) tools: Option<Vec<String>>,
}

/// A single subagent definition in the `"subagents"` array.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonSubagent {
    /// Tool name (e.g., "research_agent").
    pub(super) name: String,
    /// Description shown to the parent LLM.
    pub(super) description: String,
    /// System prompt for the subagent.
    pub(super) system_prompt: String,
    /// Provider name from the `"providers"` map.
    pub(super) provider: String,
    /// Optional model override.
    pub(super) model: Option<String>,
    /// Max LLM turns per invocation (default: 10).
    pub(super) max_iterations: Option<usize>,
    /// Max tokens per response (default: 4096).
    pub(super) max_tokens: Option<u32>,
    /// Optional tool name filter (None = inherit all parent tools).
    pub(super) tools: Option<Vec<String>>,
}

/// The `"sandbox"` object.
///
/// ```json
/// "sandbox": {
///   "disabled": ["os"],
///   "os_profile": "strict"
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonSandbox {
    /// Sandbox names to disable.
    #[serde(default)]
    pub(super) disabled: Vec<String>,
    /// OS sandbox profile: "default", "strict", "permissive".
    pub(super) os_profile: Option<String>,
    /// Per-tool sandbox policies (tool name or glob → policy overrides).
    #[serde(default)]
    pub(super) tool_policies: std::collections::HashMap<String, JsonToolPolicy>,
}

/// Per-tool policy overrides in dyson.json.
///
/// ```json
/// "web_search": {
///   "network": "allow",
///   "file_read": "deny",
///   "file_write": { "restrict_to": ["/tmp/workdir"] }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonToolPolicy {
    pub(super) network: Option<String>,
    pub(super) file_read: Option<serde_json::Value>,
    pub(super) file_write: Option<serde_json::Value>,
    pub(super) process_exec: Option<String>,
}

/// The `"workspace"` object.
///
/// Supports both new-style `backend` + `connection_string` and legacy `path`:
/// ```json
/// { "workspace": { "backend": "filesystem", "connection_string": "~/.dyson" } }
/// { "workspace": { "path": "~/.dyson" } }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonWorkspace {
    /// Backend type: "filesystem" (default).
    pub(super) backend: Option<String>,
    /// Connection string (path for filesystem).  Supports secret resolution.
    pub(super) connection_string: Option<SecretValue>,
    /// Legacy: plain path.  Falls back to this if connection_string is absent.
    pub(super) path: Option<String>,
    /// Memory tier configuration.
    pub(super) memory: Option<JsonMemory>,
}

/// The `"memory"` object inside `"workspace"`.
///
/// ```json
/// {
///   "memory": {
///     "limits": { "MEMORY.md": 2500 },
///     "overflow_factor": 1.35,
///     "nudge_interval": 5
///   }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonMemory {
    /// Per-file soft character targets.
    pub(super) limits: Option<std::collections::HashMap<String, usize>>,
    /// Multiplier that turns a soft target into a hard ceiling.
    pub(super) overflow_factor: Option<f32>,
    /// Nudge interval in turns (0 = disabled).
    pub(super) nudge_interval: Option<usize>,
}

/// The `"chat_history"` object.
///
/// ```json
/// { "chat_history": { "backend": "disk", "connection_string": "~/.dyson/chats" } }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JsonChatHistory {
    /// Backend type: "disk" (default).
    pub(super) backend: Option<String>,
    /// Connection string (directory path for disk).  Supports secret resolution.
    pub(super) connection_string: Option<SecretValue>,
}
