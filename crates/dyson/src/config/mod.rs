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
//       { "type": "my_bot", "api_key": "literal-key" }
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
    /// Multiple controllers can run concurrently (e.g., terminal + a chat bot).
    /// If empty, defaults to a single terminal controller.
    pub controllers: Vec<ControllerConfig>,

    /// Sandbox configuration.
    pub sandbox: SandboxConfig,

    /// Workspace configuration (backend + connection string).
    pub workspace: WorkspaceConfig,

    /// Chat history configuration (backend + connection string).
    pub chat_history: ChatHistoryConfig,

    /// Audio transcriber configuration (provider + model).
    ///
    /// When present, the specified transcriber is used for audio.
    /// When `None`, defaults to `whisper-cli` with the "base" model.
    pub transcriber: Option<TranscriberConfig>,

    /// Web search configuration (provider + API key).
    ///
    /// When present, the `web_search` built-in tool is registered.
    /// When `None`, the tool is simply absent.
    pub web_search: Option<WebSearchConfig>,

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
///     "os_profile": "strict",
///     "tool_policies": {
///       "web_search": { "network": "allow", "file_read": "deny" },
///       "mcp__*": { "network": "allow", "file_write": "deny" }
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// Sandbox names to disable.  Everything not in this list is active.
    ///
    /// Known sandboxes: "os"
    /// Future: "audit", "ratelimit"
    pub disabled: Vec<String>,

    /// OS sandbox profile: "default", "strict", or "permissive".
    ///
    /// Only used as a backward-compatibility fallback when `tool_policies`
    /// is empty.  When `tool_policies` is set, policies control everything.
    pub os_profile: Option<String>,

    /// Per-tool sandbox policies.
    ///
    /// Keys are tool names or glob patterns (e.g., "mcp__*").
    /// Values override the default policy for that tool.
    /// Unspecified tools get sensible defaults (see `sandbox::policy`).
    pub tool_policies: std::collections::HashMap<String, crate::sandbox::policy::ToolPolicyConfig>,
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
///   "type": "my_controller",
///   "api_key": { "resolver": "insecure_env", "name": "MY_API_KEY" },
///   "channel_ids": [123456789]
/// }
/// ```
///
/// Each controller reads its own fields from `config` using serde:
/// ```ignore
/// #[derive(Deserialize)]
/// struct MyControllerConfig {
///     api_key: SecretValue,
///     channel_ids: Option<Vec<i64>>,
/// }
/// let cfg: MyControllerConfig = serde_json::from_value(config.config.clone())?;
/// ```
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Controller type identifier (e.g. "terminal").
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

    /// Maximum retries on transient LLM failures: HTTP 429/529, network
    /// errors, and empty responses (no text and no tool calls).  Each
    /// retry uses exponential backoff with jitter and does not advance
    /// the per-turn iteration counter.  Defaults to 6 — upstream rate
    /// limits on budget providers (OpenRouter → DeepSeek) often take
    /// ~30-60s to clear, so a longer total backoff window (~63s at
    /// defaults) prevents premature give-ups.
    pub max_retries: usize,

    /// Maximum LLM requests in flight at once for this provider.
    ///
    /// Multiple controllers (telegram + http + swarm) and background agents
    /// (dreams, reflection, learning synthesis) all share one provider
    /// client.  Without a cap they fan out concurrently and trip per-minute
    /// rate limits.  The semaphore wraps the retry decorator, so permits
    /// are held across backoff sleeps — sticky 429s serialise instead of
    /// thundering.  Defaults to 4; set to 0 to disable the cap.
    pub max_concurrent_llm_calls: usize,

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

    /// Advisor model in `provider/model` format (e.g. `"claude/claude-opus-4-6"`).
    ///
    /// When set, the executor can consult a stronger model for complex
    /// decisions.  If both executor and advisor are Anthropic, uses the
    /// native `advisor_20260301` API tool.  Otherwise, spawns a subagent.
    /// Skipped when the advisor resolves to the currently loaded model.
    pub smartest_model: Option<String>,

    /// Name of a provider from the `"providers"` map to use for image
    /// generation.
    ///
    /// When set, the `image_generate` built-in tool is registered using
    /// the referenced provider's API key and model.  The provider must
    /// support image generation (currently: Gemini).
    ///
    /// ```json
    /// {
    ///   "agent": { "image_generation_provider": "gemini" }
    /// }
    /// ```
    pub image_generation_provider: Option<String>,

    /// Model override for image generation.
    ///
    /// When set, the `image_generate` tool uses this model instead of the
    /// image generation provider's default model.  Useful when the provider
    /// is also used for chat with a different default model.
    ///
    /// ```json
    /// {
    ///   "agent": { "image_generation_model": "gemini-3-pro-image-preview" }
    /// }
    /// ```
    pub image_generation_model: Option<String>,

    /// Context compaction configuration.
    ///
    /// Controls automatic conversation compaction.  When the estimated context
    /// size exceeds `compaction.threshold()`, the agent runs a Hermes-style
    /// five-phase algorithm that preserves the head and tail of the conversation
    /// while summarising the middle.  Defaults to sensible values (200k window,
    /// 50% threshold).
    pub compaction: CompactionConfig,

    /// Rate limiting configuration.
    ///
    /// Limits the number of user messages processed per time window.
    /// Applied per agent instance.  `None` = no rate limit (default).
    pub rate_limit: Option<RateLimitConfig>,
}

// ---------------------------------------------------------------------------
// RateLimitConfig
// ---------------------------------------------------------------------------

/// Per-agent rate limiting configuration.
///
/// Controls how many user messages an agent will process within a sliding
/// time window.  Configured in dyson.json under `agent.rate_limit`.
///
/// ```json
/// {
///   "agent": {
///     "rate_limit": {
///       "max_messages": 30,
///       "window_secs": 60
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Maximum messages allowed within the window.
    pub max_messages: usize,
    /// Window duration in seconds.
    pub window_secs: u64,
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
    /// Ollama Cloud API (cloud-hosted models via ollama.com).
    OllamaCloud,
    /// Google Gemini API (Nano Banana image generation, future: chat).
    Gemini,
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
///       "models": ["claude-sonnet-4-20250514", "claude-opus-4-20250514"],
///       "api_key": "sk-..."
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Provider backend type (anthropic, openai, claude-code, codex).
    pub provider_type: LlmProvider,
    /// Available model identifiers.  The first entry is the default.
    pub models: Vec<String>,
    /// Resolved API key (empty string for CLI-based providers).
    /// Uses `Credential` to zeroize the key from memory on drop.
    pub api_key: crate::auth::Credential,
    /// Optional base URL override for the provider API.
    pub base_url: Option<String>,
}

impl ProviderConfig {
    /// The default model — first entry in the `models` list.
    ///
    /// Panics if `models` is empty (should never happen — the loader
    /// always populates at least one model from registry defaults).
    pub fn default_model(&self) -> &str {
        self.models
            .first()
            .expect("ProviderConfig.models must not be empty (loader guarantees at least one)")
    }
}

// ---------------------------------------------------------------------------
// SkillConfig
// ---------------------------------------------------------------------------

/// Configuration for a single skill (tool bundle).
#[derive(Debug, Clone)]
pub enum SkillConfig {
    Mcp(Box<McpConfig>),
    Local(LocalSkillConfig),
    Builtin(BuiltinSkillConfig),
    /// Subagent skill — spawns child agents as tools.
    ///
    /// Each subagent is a Tool that creates a fresh Agent with its own
    /// LlmClient (potentially a different model/provider), runs it to
    /// completion, and returns the result.  Subagents share the parent's
    /// sandbox for security and workspace for memory.
    Subagent(SubagentSkillConfig),
}

#[derive(Clone, Debug)]
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
        /// When true, wrap the subprocess in `bwrap` (Linux) with a
        /// read-only root, tmpfs `/tmp`, a PID namespace, and
        /// `--die-with-parent`.  Network is left shared because most
        /// MCP servers need it; set `sandbox_deny_network: true` to
        /// isolate.
        ///
        /// When false or omitted, the subprocess runs unsandboxed with
        /// full Dyson-process privileges — a warning is logged at load.
        sandbox: bool,
        /// When `sandbox` is true, also unshare the network namespace.
        /// Defaults to false (shared) so servers that legitimately need
        /// APIs keep working.
        sandbox_deny_network: bool,
    },
    /// POST JSON-RPC to an HTTP endpoint (Streamable HTTP MCP).
    /// Used by servers like Context7, Stripe MCP, etc.
    Http {
        url: String,
        headers: std::collections::HashMap<String, String>,
        /// Optional OAuth 2.0 configuration for servers that require
        /// interactive authorization (e.g., GitHub Copilot MCP).
        /// When set, Dyson runs the OAuth Authorization Code + PKCE flow
        /// and attaches Bearer tokens automatically.
        auth: Option<McpAuthConfig>,
    },
}

// ---------------------------------------------------------------------------
// MCP OAuth configuration
// ---------------------------------------------------------------------------

/// OAuth 2.0 configuration for an MCP HTTP server.
///
/// When present on an `McpTransportConfig::Http`, Dyson runs the OAuth
/// Authorization Code + PKCE flow instead of using static headers.
///
/// ## Minimal config (auto-discovery + DCR)
///
/// ```json
/// {
///   "url": "https://mcp.example.com/mcp",
///   "auth": { "type": "oauth", "scopes": ["read"] }
/// }
/// ```
///
/// ## Full config (pre-registered client, no discovery)
///
/// ```json
/// {
///   "url": "https://mcp.example.com/mcp",
///   "auth": {
///     "type": "oauth",
///     "client_id": "my-client-id",
///     "client_secret": { "resolver": "insecure_env", "name": "CLIENT_SECRET" },
///     "scopes": ["read", "write"],
///     "authorization_url": "https://auth.example.com/authorize",
///     "token_url": "https://auth.example.com/token"
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct McpAuthConfig {
    /// `None` = use Dynamic Client Registration.
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub redirect_uri: Option<String>,
    /// Overrides (skip well-known discovery when set).
    pub authorization_url: Option<String>,
    pub token_url: Option<String>,
    pub registration_url: Option<String>,
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
// SubagentSkillConfig
// ---------------------------------------------------------------------------

/// Configuration for the subagent skill — bundles one or more subagent
/// definitions into a single skill.
///
/// Each entry in `agents` becomes a separate `SubagentTool` that the
/// parent LLM can invoke.  Subagents can use different providers and
/// models from the parent agent.
///
/// ```json
/// {
///   "skills": {
///     "subagents": [
///       {
///         "name": "research_agent",
///         "description": "Research specialist for web research tasks.",
///         "system_prompt": "You are a research specialist.",
///         "provider": "gpt",
///         "max_iterations": 15
///       }
///     ]
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct SubagentSkillConfig {
    /// Individual subagent definitions.
    pub agents: Vec<SubagentAgentConfig>,
}

/// Configuration for a single subagent.
///
/// Maps to one `SubagentTool` in the parent agent.  The `provider` field
/// references a named entry in the top-level `"providers"` map, allowing
/// each subagent to use a different LLM backend and model.
///
/// ## Tool filtering
///
/// By default, subagents inherit **all** the parent's tools (builtins +
/// MCP + local skills) except other subagent tools (which prevents
/// recursive spawning).  The optional `tools` field filters this to a
/// specific subset by tool name.
#[derive(Debug, Clone)]
pub struct SubagentAgentConfig {
    /// Tool name exposed to the parent LLM (e.g., "research_agent").
    ///
    /// Must be a valid identifier (lowercase, underscores) — it appears
    /// in tool_use blocks and log output.
    pub name: String,

    /// Human-readable description shown to the parent LLM.
    ///
    /// The parent LLM uses this to decide *when* to delegate to this
    /// subagent.  Be specific about its specialty.
    pub description: String,

    /// System prompt for the subagent.
    ///
    /// Defines the subagent's personality, expertise, and behavioral
    /// guidelines.  This is separate from the parent's system prompt.
    pub system_prompt: String,

    /// Provider name from the `"providers"` map in dyson.json.
    ///
    /// Looked up at skill construction time to resolve the provider type,
    /// API key, and base URL.
    pub provider: String,

    /// Optional model override (defaults to the provider's default model).
    pub model: Option<String>,

    /// Maximum LLM turns per subagent invocation (default: 10).
    pub max_iterations: Option<usize>,

    /// Maximum tokens per LLM response (default: 4096).
    pub max_tokens: Option<u32>,

    /// Optional tool name filter.
    ///
    /// When `Some`, only tools whose names are in this list are available
    /// to the subagent.  When `None`, all parent tools are inherited
    /// (minus subagent tools).
    ///
    /// Tool names that don't match any parent tool are silently ignored.
    pub tools: Option<Vec<String>>,

    /// Optional system-prompt fragment appended to the parent's subagent
    /// prompt whenever this subagent is present.  Used by built-ins like
    /// `verifier` to describe when/how they should be invoked.  Not
    /// currently deserialized from `dyson.json`.
    pub injects_protocol: Option<String>,
}

// ---------------------------------------------------------------------------
// WorkspaceConfig
// ---------------------------------------------------------------------------

/// Configuration for the workspace backend.
///
/// ```json
/// {
///   "workspace": {
///     "backend": "filesystem",
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
    /// Backend type.  Currently supported: "filesystem".
    pub backend: String,

    /// Connection string (path for filesystem).  Resolved via secret system.
    /// Uses `Credential` because connection strings can contain passwords
    /// (e.g., database URLs with embedded credentials).
    pub connection_string: crate::auth::Credential,

    /// Memory tier configuration: character limits and nudge interval.
    pub memory: MemoryConfig,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            backend: "filesystem".into(),
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
///       "limits": { "MEMORY.md": 2500, "USER.md": 1375 },
///       "overflow_factor": 1.35,
///       "nudge_interval": 7
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Per-file soft character targets.  Keys are file names (e.g. "MEMORY.md").
    ///
    /// These are **soft targets**, not hard caps.  The curator aims for the
    /// target but is allowed to overflow up to `target * overflow_factor`
    /// (the "hard ceiling") when the extra characters carry genuine signal.
    /// 2,700 chars of valuable context is better than 2,470 chars of
    /// truncated context — only the ceiling is a hard refusal.
    ///
    /// Files not listed here have no limit.
    pub limits: std::collections::HashMap<String, usize>,

    /// Multiplier that converts a soft target into a hard ceiling.
    ///
    /// `ceiling = soft_target * overflow_factor`.  Default `1.35` allows
    /// ~35% overflow so curation can preserve valuable signal instead of
    /// truncating it to hit the target exactly.
    pub overflow_factor: f32,

    /// Inject a memory maintenance nudge every N turns.  0 = disabled.
    pub nudge_interval: usize,
}

impl MemoryConfig {
    /// Compute the hard ceiling for a file given its soft target.
    pub fn ceiling_for(&self, file: &str) -> Option<usize> {
        let target = *self.limits.get(file)?;
        let ceiling = (target as f32 * self.overflow_factor).round() as usize;
        Some(ceiling.max(target))
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        let mut limits = std::collections::HashMap::new();
        // Soft targets — curation aims here but may overflow up to
        // `target * overflow_factor` when the extra chars are signal.
        limits.insert("MEMORY.md".into(), 2500);
        limits.insert("USER.md".into(), 1375);
        Self {
            limits,
            overflow_factor: 1.35,
            nudge_interval: 7,
        }
    }
}

// ---------------------------------------------------------------------------
// CompactionConfig
// ---------------------------------------------------------------------------

/// Configuration for the Hermes-style context compressor.
///
/// Controls when and how the agent compacts its conversation history
/// to stay within the model's context window while preserving critical
/// context at both ends of the conversation.
///
/// The algorithm has five phases:
///   1. **Tool output pruning** — replace old tool results outside protected
///      regions with placeholders (no LLM call needed).
///   2. **Region identification** — protect the first N messages (head) and
///      the most recent messages within a token budget (tail).
///   3. **Structured summarisation** — summarise only the middle section
///      via LLM (Goal / Progress / Decisions / Files / Next Steps).
///   4. **Reassembly** — head + `[Context Summary]` + tail.
///   5. **Orphan repair** — fix broken tool_use / tool_result pairs at the
///      boundaries.
///
/// ```json
/// {
///   "agent": {
///     "compaction": {
///       "context_window": 200000,
///       "threshold_ratio": 0.50,
///       "protect_head": 3,
///       "protect_tail_tokens": 20000,
///       "summary_min_tokens": 2000,
///       "summary_max_tokens": 12000,
///       "summary_target_ratio": 0.20
///     }
///   }
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    /// Model's context window in estimated tokens.
    /// The compaction threshold is `context_window * threshold_ratio`.
    pub context_window: usize,

    /// Fraction of `context_window` at which to trigger compaction (default 0.50).
    pub threshold_ratio: f64,

    /// Number of messages to always keep at the start of the conversation.
    /// These are never summarised (default 3).
    pub protect_head: usize,

    /// Estimated token budget for messages to protect at the end (default 20,000).
    /// The most recent messages fitting within this budget are kept verbatim.
    pub protect_tail_tokens: usize,

    /// Minimum tokens for the summary output (default 2,000).
    pub summary_min_tokens: usize,

    /// Maximum tokens for the summary output (default 12,000).
    pub summary_max_tokens: usize,

    /// Target ratio of summary size to middle section size (default 0.20).
    /// The actual max_tokens for the summarisation call is
    /// `clamp(middle_tokens * summary_target_ratio, summary_min_tokens, summary_max_tokens)`.
    pub summary_target_ratio: f64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window: 200_000,
            threshold_ratio: 0.50,
            protect_head: 3,
            protect_tail_tokens: 20_000,
            summary_min_tokens: 2_000,
            summary_max_tokens: 12_000,
            summary_target_ratio: 0.20,
        }
    }
}

impl CompactionConfig {
    /// The effective token threshold at which compaction triggers.
    pub fn threshold(&self) -> usize {
        (self.context_window as f64 * self.threshold_ratio) as usize
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
// TranscriberConfig
// ---------------------------------------------------------------------------

/// Configuration for the audio transcription backend.
///
/// When present in settings, the specified transcriber is used for audio.
/// When absent, defaults to `whisper-cli` with the "base" model.
///
/// ```json
/// {
///   "transcriber": {
///     "provider": "whisper-cli",
///     "model": "small"
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TranscriberConfig {
    /// Transcriber provider: "whisper-cli" (default).
    pub provider: String,

    /// Model name/size (e.g. "tiny", "base", "small", "medium", "large").
    /// Provider-specific — for whisper-cli this is the Whisper model size.
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSearchConfig
// ---------------------------------------------------------------------------

/// Configuration for the web search tool.
///
/// When present in settings, the `web_search` built-in tool is registered
/// with the configured search provider.  When absent, the tool doesn't exist.
///
/// ```json
/// {
///   "web_search": {
///     "provider": "brave",
///     "api_key": { "resolver": "insecure_env", "name": "BRAVE_API_KEY" }
///   }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WebSearchConfig {
    /// Search provider: "brave" (default).  Future: "tavily", "searxng".
    pub provider: String,

    /// API key for the search provider.  Resolved via secret system.
    pub api_key: crate::auth::Credential,

    /// Optional base URL override (e.g. for self-hosted SearXNG).
    pub base_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            // No hardcoded model default.  The config loader rejects configs
            // that don't resolve to a concrete `agent.model` — users must
            // configure their model explicitly so Dyson never silently bills
            // a model they didn't choose.
            model: String::new(),
            max_iterations: 20,
            max_retries: 6,
            max_concurrent_llm_calls: 4,
            max_tokens: 8192,
            system_prompt: "You are Dyson, a capable AI assistant.  You can use \
                            tools to help answer questions and complete tasks.\n\n\
                            ## Grounding\n\n\
                            Every factual claim about files, code, counts, bugs, \
                            versions, module locations, or quoted text must come \
                            from a tool call made in this session, or be marked \
                            explicitly as [unverified] or [inferred].  Unsourced \
                            specifics are hallucinations.  Prior-context or \
                            training-data recall is a hypothesis to test with a \
                            tool call, not a source to cite.\n\n\
                            Cheap verification is mandatory.  If a single \
                            `list_files`, `read_file`, `search_files`, or `bash` \
                            invocation can settle a claim, run it.  Specifically:\n\
                            - Line counts → `bash: wc -l`, never estimated.\n\
                            - File counts → `list_files` or `bash: ls | wc -l`, \
                              never eyeballed.\n\
                            - \"File X contains Y\" → `read_file` or \
                              `search_files`, never from memory.\n\
                            - \"Bug B exists in file F\" → `read_file` the \
                              relevant lines first.\n\
                            - Citing a document by name (e.g. README.md, \
                              prompt.md, TODO.md) → confirm it exists with \
                              `list_files` or `read_file` before citing it.\n\n\
                            Before finishing a response that contains factual \
                            claims, re-read it and check each specific number, \
                            filename, and quoted string against a tool result \
                            from this session.  Flag contradictions (e.g. \
                            \"19 X\" and \"20+ X\" in the same answer) and \
                            resolve them before sending."
                .into(),
            api_key: crate::auth::Credential::new(String::new()),
            provider: LlmProvider::Anthropic,
            base_url: None,
            smartest_model: None,
            image_generation_provider: None,
            image_generation_model: None,
            compaction: CompactionConfig::default(),
            rate_limit: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            agent: AgentSettings::default(),
            providers: std::collections::HashMap::new(),
            skills: vec![SkillConfig::Builtin(BuiltinSkillConfig { tools: vec![] })],
            controllers: vec![],
            sandbox: SandboxConfig::default(),
            workspace: WorkspaceConfig::default(),
            chat_history: ChatHistoryConfig::default(),
            transcriber: None,
            web_search: None,
            dangerous_no_sandbox: false,
        }
    }
}
