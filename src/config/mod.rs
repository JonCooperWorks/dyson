// ===========================================================================
// Configuration — portable settings that drive the entire agent.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the runtime `Settings` struct and its sub-types.  Every tunable
//   knob in Dyson — model name, max iterations, which skills to load,
//   which controllers to run — lives here.
//
// Module layout:
//   mod.rs      — Settings, AgentSettings, ControllerConfig, etc. (this file)
//   loader.rs   — JSON config loader (dyson.json)
//
// Config format: JSON.
//   Dyson uses JSON because it handles nesting naturally.  Each controller
//   declares its own fields inside its object — no flat-struct problem,
//   no TOML array-of-tables awkwardness.
//
//   ```json
//   {
//     "agent": { "provider": "claude-code", "model": "sonnet" },
//     "controllers": [
//       { "type": "terminal" },
//       { "type": "telegram", "bot_token": "literal-token" }
//     ]
//   }
//   ```
//
// Controller config is opaque:
//   `ControllerConfig` has a `type` field and a `serde_json::Value` blob
//   for everything else.  Each controller implementation parses its own
//   fields from that blob.  Adding a new controller type never touches
//   the config structs — it just defines its own config shape.
//
// Resolution order (highest priority wins):
//   1. CLI flags (--provider, --base-url)
//   2. dyson.json in CWD or ~/.config/dyson/dyson.json
//   3. Environment variables (ANTHROPIC_API_KEY, etc.)
//   4. Hardcoded defaults
// ===========================================================================

pub mod loader;

// ---------------------------------------------------------------------------
// Settings — the top-level config struct.
// ---------------------------------------------------------------------------

/// Complete, resolved configuration for a Dyson session.
///
/// Built once at startup from config files + env vars + CLI flags, then
/// passed (by reference) to the agent and all subsystems.  Immutable after
/// construction — no runtime config changes.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Core agent behavior: model, limits, system prompt, API key.
    pub agent: AgentSettings,

    /// Which skills (tool bundles) to load.
    pub skills: Vec<SkillConfig>,

    /// Controllers — how Dyson interacts with the outside world.
    ///
    /// Each entry in the `"controllers"` JSON array becomes one of these.
    /// Multiple controllers can run concurrently (e.g., terminal + Telegram).
    /// If empty, defaults to a single terminal controller.
    pub controllers: Vec<ControllerConfig>,
}

// ---------------------------------------------------------------------------
// ControllerConfig — type + opaque config blob.
// ---------------------------------------------------------------------------

/// Configuration for a single controller instance.
///
/// The `type` field selects the controller implementation.  The `config`
/// blob contains controller-specific fields that only that implementation
/// knows how to parse.  This is fully extensible — adding a new controller
/// type never touches this struct.
///
/// ```json
/// {
///   "type": "telegram",
///   "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
///   "allowed_chat_ids": [123456789]
/// }
/// ```
///
/// The controller reads its fields from `config` using serde:
/// ```ignore
/// #[derive(Deserialize)]
/// struct TelegramControllerConfig {
///     bot_token: SecretValue,
///     allowed_chat_ids: Option<Vec<i64>>,
/// }
/// let tg: TelegramControllerConfig = serde_json::from_value(config.config.clone())?;
/// ```
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Controller type: "terminal", "telegram", etc.
    pub controller_type: String,

    /// Controller-specific configuration as a raw JSON value.
    ///
    /// Each controller implementation deserializes this into its own
    /// typed config struct.  The config layer doesn't interpret it.
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// AgentSettings
// ---------------------------------------------------------------------------

/// Knobs that control the agent loop and LLM interaction.
#[derive(Debug, Clone)]
pub struct AgentSettings {
    /// LLM model identifier (e.g. "claude-sonnet-4-20250514").
    pub model: String,

    /// Maximum number of LLM turns before the agent gives up.
    pub max_iterations: usize,

    /// Maximum tokens the LLM can generate per turn.
    pub max_tokens: u32,

    /// Base system prompt.  Skill-specific prompt fragments are appended
    /// to this by the agent at runtime.
    pub system_prompt: String,

    /// API key for the LLM provider (resolved at load time).
    pub api_key: String,

    /// Which LLM provider to use.
    pub provider: LlmProvider,

    /// Optional base URL override for the LLM API.
    pub base_url: Option<String>,
}

// ---------------------------------------------------------------------------
// LlmProvider
// ---------------------------------------------------------------------------

/// The LLM provider backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmProvider {
    /// Anthropic Messages API (Claude models).
    Anthropic,
    /// OpenAI Chat Completions API (GPT, Ollama, vLLM, Together, etc.).
    OpenAi,
    /// Locally installed `claude` CLI (no API key needed).
    ClaudeCode,
}

// ---------------------------------------------------------------------------
// SkillConfig
// ---------------------------------------------------------------------------

/// Configuration for a single skill (tool bundle).
#[derive(Debug, Clone)]
pub enum SkillConfig {
    Mcp(McpConfig),
    Local(LocalSkillConfig),
    Builtin(BuiltinSkillConfig),
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub name: String,
    pub transport: McpTransportConfig,
}

#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    /// Spawn a local process and communicate over stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: std::collections::HashMap<String, String>,
    },
    /// POST JSON-RPC to an HTTP endpoint (Streamable HTTP MCP).
    /// Used by servers like Context7, Stripe MCP, etc.
    Http {
        url: String,
        headers: std::collections::HashMap<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct LocalSkillConfig {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct BuiltinSkillConfig {
    pub tools: Vec<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".into(),
            max_iterations: 20,
            max_tokens: 8192,
            system_prompt: "You are Dyson, a capable AI assistant. You can use tools to help \
                            answer questions and complete tasks."
                .into(),
            api_key: String::new(),
            provider: LlmProvider::Anthropic,
            base_url: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agent: AgentSettings::default(),
            skills: vec![SkillConfig::Builtin(BuiltinSkillConfig {
                tools: vec![],
            })],
            controllers: vec![],
        }
    }
}
