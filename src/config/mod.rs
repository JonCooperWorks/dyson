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

pub mod hot_reload;
pub mod loader;
pub mod migrate;

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

    /// Named provider configurations from the `"providers"` JSON map.
    ///
    /// Each entry is a fully-resolved provider (API key resolved, type
    /// parsed).  The active provider's fields are flattened into `agent`
    /// at load time, but the full map is available for runtime switching
    /// (e.g. `/model <name>` commands).
    pub providers: std::collections::HashMap<String, ProviderConfig>,

    /// Which skills (tool bundles) to load.
    pub skills: Vec<SkillConfig>,

    /// Controllers — how Dyson interacts with the outside world.
    ///
    /// Each entry in the `"controllers"` JSON array becomes one of these.
    /// Multiple controllers can run concurrently (e.g., terminal + Telegram).
    /// If empty, defaults to a single terminal controller.
    pub controllers: Vec<ControllerConfig>,

    /// Sandbox configuration.
    pub sandbox: SandboxConfig,

    /// Workspace configuration (backend + connection string).
    pub workspace: WorkspaceConfig,

    /// Chat history configuration (backend + connection string).
    pub chat_history: ChatHistoryConfig,

    /// Whether `--dangerous-no-sandbox` was passed on the CLI.
    ///
    /// This is the ONLY way to disable all sandboxes.  It cannot be set
    /// from config — only from the command line, as a conscious decision.
    pub dangerous_no_sandbox: bool,
}

// ---------------------------------------------------------------------------
// SandboxConfig
// ---------------------------------------------------------------------------

/// Configures which sandboxes are active.
///
/// By default, ALL sandboxes are enabled.  You disable specific ones
/// in the config.  `DangerousNoSandbox` (disable everything) is only
/// available via the `--dangerous-no-sandbox` CLI flag — it cannot be
/// set from config, because config files can be committed to repos and
/// shared, and "no sandbox" should always be a conscious, local decision.
///
/// ```json
/// {
///   "sandbox": {
///     "disabled": ["os"],
///     "os_profile": "strict"
///   }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// Sandbox names to disable.  Everything not in this list is active.
    ///
    /// Known sandboxes: "os"
    /// Future: "file", "network", "audit", "ratelimit"
    pub disabled: Vec<String>,

    /// OS sandbox profile: "default", "strict", or "permissive".
    ///
    /// The OS sandbox is enabled by default.  The profile controls how
    /// restrictive it is:
    /// - "default" — deny network, allow writes to cwd + /tmp
    /// - "strict" — deny network, deny all writes outside cwd
    /// - "permissive" — allow everything (just wraps in sandbox-exec)
    pub os_profile: Option<String>,
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
    /// Uses `Credential` to zeroize the key from memory on drop.
    pub api_key: crate::auth::Credential,

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
    /// OpenRouter unified API (200+ models via OpenAI-compatible endpoint).
    OpenRouter,
    /// Locally installed `claude` CLI (no API key needed).
    ClaudeCode,
    /// Locally installed `codex` CLI (OpenAI Codex, no API key needed).
    Codex,
}

// ---------------------------------------------------------------------------
// ProviderConfig — a named provider entry from the "providers" map.
// ---------------------------------------------------------------------------

/// A named, fully-resolved provider configuration.
///
/// Defined in the `"providers"` map in dyson.json.  At load time, the
/// active provider's fields are copied into `AgentSettings`.  The full
/// map is kept on `Settings` for runtime switching (e.g. `/model`).
///
/// ```json
/// {
///   "providers": {
///     "claude": {
///       "type": "anthropic",
///       "model": "claude-sonnet-4-20250514",
///       "api_key": "sk-..."
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Provider backend type (anthropic, openai, claude-code, codex).
    pub provider_type: LlmProvider,
    /// Model identifier (e.g. "claude-sonnet-4-20250514", "gpt-4o").
    pub model: String,
    /// Resolved API key (empty string for CLI-based providers).
    /// Uses `Credential` to zeroize the key from memory on drop.
    pub api_key: crate::auth::Credential,
    /// Optional base URL override for the provider API.
    pub base_url: Option<String>,
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
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// Configuration for the workspace backend.
///
/// ```json
/// {
///   "workspace": {
///     "backend": "openclaw",
///     "connection_string": "~/.dyson"
///   }
/// }
/// ```
///
/// The `connection_string` supports the secret resolver scheme:
/// ```json
/// { "connection_string": { "resolver": "insecure_env", "name": "WORKSPACE_DIR" } }
/// ```
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Backend type.  Currently supported: "openclaw".
    pub backend: String,

    /// Connection string (path for openclaw).  Resolved via secret system.
    /// Uses `Credential` because connection strings can contain passwords
    /// (e.g., database URLs with embedded credentials).
    pub connection_string: crate::auth::Credential,

    /// Memory tier configuration: character limits and nudge interval.
    pub memory: MemoryConfig,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            backend: "openclaw".into(),
            connection_string: crate::auth::Credential::new("~/.dyson".into()),
            memory: MemoryConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryConfig
// ---------------------------------------------------------------------------

/// Configuration for the tiered memory system.
///
/// Controls character limits on Tier 1 files (always in context) and the
/// nudge interval for periodic memory maintenance reminders.
///
/// ```json
/// {
///   "workspace": {
///     "memory": {
///       "limits": { "MEMORY.md": 2200, "USER.md": 1375 },
///       "nudge_interval": 5
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Per-file character limits.  Keys are file names (e.g. "MEMORY.md").
    /// Files not listed here have no limit.
    pub limits: std::collections::HashMap<String, usize>,

    /// Inject a memory maintenance nudge every N turns.  0 = disabled.
    pub nudge_interval: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        let mut limits = std::collections::HashMap::new();
        limits.insert("MEMORY.md".into(), 2200);
        limits.insert("USER.md".into(), 1375);
        Self {
            limits,
            nudge_interval: 5,
        }
    }
}

// ---------------------------------------------------------------------------
// ChatHistoryConfig
// ---------------------------------------------------------------------------

/// Configuration for the chat history backend.
///
/// ```json
/// {
///   "chat_history": {
///     "backend": "disk",
///     "connection_string": "~/.dyson/chats"
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ChatHistoryConfig {
    /// Backend type.  Currently supported: "disk".
    pub backend: String,

    /// Connection string (directory path for disk).  Resolved via secret system.
    /// Uses `Credential` because connection strings can contain passwords.
    pub connection_string: crate::auth::Credential,
}

impl Default for ChatHistoryConfig {
    fn default() -> Self {
        Self {
            backend: "disk".into(),
            connection_string: crate::auth::Credential::new("~/.dyson/chats".into()),
        }
    }
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
            api_key: crate::auth::Credential::new(String::new()),
            provider: LlmProvider::Anthropic,
            base_url: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agent: AgentSettings::default(),
            providers: std::collections::HashMap::new(),
            skills: vec![SkillConfig::Builtin(BuiltinSkillConfig {
                tools: vec![],
            })],
            controllers: vec![],
            sandbox: SandboxConfig::default(),
            workspace: WorkspaceConfig::default(),
            chat_history: ChatHistoryConfig::default(),
            dangerous_no_sandbox: false,
        }
    }
}
