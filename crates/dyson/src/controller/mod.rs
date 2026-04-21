// ===========================================================================
// Controller — the input/output lifecycle for an agent session.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `Controller` trait — the top-level abstraction for how
//   Dyson interacts with the outside world.  A controller owns the full
//   lifecycle: receiving user input, running the agent, and delivering
//   output.  Multiple controllers can run concurrently.
//
// Module layout:
//   mod.rs      — Controller trait, Output trait, shared helpers (this file)
//   terminal.rs — Interactive terminal REPL
//   telegram.rs — Telegram bot
//
// Why "Controller" instead of "UI"?
//   "UI" implies visual rendering.  But a controller does more than render:
//   - It sources input (stdin, messages, HTTP requests, cron)
//   - It manages the agent lifecycle (create, run, conversation state)
//   - It delivers output (text streams, message edits, webhooks)
//   - It enforces access control (allowed users, API keys, etc.)
//
//   A chat bot isn't a "UI" — it's a controller that bridges a messaging
//   protocol to Dyson's agent loop.  The same applies to a terminal REPL,
//   a mobile app backend, or an HTTP API server.
//
// How controllers fit in the architecture:
//
//   dyson.json "controllers" array
//     │
//     ▼
//   main.rs reads config, creates Controller instances
//     │
//     ├── TerminalController::run()   ← interactive REPL
//     ├── (other controllers)         ← chat bots, HTTP APIs, etc.
//           │
//           ▼
//         Each controller creates its own Agent and Output
//         per session/message/request
//
// Multiple controllers:
//   Dyson supports running multiple controllers simultaneously.
//   Each controller runs as a concurrent tokio task.  They share the
//   same agent settings but maintain independent conversation state.
//
// The Output trait:
//   Output is the rendering half of the controller.  It's separated out
//   because the agent loop needs a render target (`&mut dyn Output`) but
//   doesn't care about input sourcing or lifecycle management.  The
//   controller creates an Output instance and passes it to the agent.
// ===========================================================================

pub mod background;
pub mod http;
pub mod recording;
#[cfg(feature = "dangerous_swarm")]
pub mod swarm;
pub mod telegram;
pub mod terminal;

use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::error::DysonError;
use crate::tool::{CheckpointEvent, ToolOutput};

// ---------------------------------------------------------------------------
// Controller trait
// ---------------------------------------------------------------------------

/// A top-level lifecycle manager for agent interaction.
///
/// Controllers own the full loop: receive input → run agent → deliver output.
/// Each controller type represents a different interaction channel
/// (terminal, chat bots, HTTP APIs, mobile backends, etc.).
///
/// ## Lifecycle
///
/// ```text
/// main.rs creates controllers from config
///   → controller.run(settings).await
///     → (blocks until the controller shuts down)
/// ```
///
/// ## Concurrency
///
/// Multiple controllers run as concurrent tokio tasks.  Each is independent:
/// separate agent instances, separate conversation state, separate I/O.
#[async_trait::async_trait]
pub trait Controller: Send {
    /// Human-readable name for logging (e.g., "terminal").
    fn name(&self) -> &str;

    /// Run the controller.  Blocks until shutdown (Ctrl-C, bot disconnect, etc.).
    ///
    /// The controller is responsible for:
    /// 1. Creating an `Agent` from the settings
    /// 2. Sourcing user input (stdin, messages, HTTP requests)
    /// 3. Running `agent.run()` with an appropriate `Output`
    /// 4. Delivering the response to the user
    ///
    /// The `registry` is shared across all controllers — all controllers
    /// use the same LLM client instances and rate-limit counters.
    async fn run(
        &self,
        settings: &Settings,
        registry: &std::sync::Arc<ClientRegistry>,
    ) -> crate::Result<()>;

    /// Optional system prompt fragment contributed by this controller.
    ///
    /// Appended to the agent's system prompt so the LLM knows about
    /// controller-specific constraints (e.g. message length limits,
    /// formatting restrictions).
    fn system_prompt(&self) -> Option<&str> {
        None
    }
}

// ---------------------------------------------------------------------------
// Agent builder — shared logic for all controllers.
// ---------------------------------------------------------------------------

/// Whether an agent session is private (full access) or public (restricted).
///
/// Controllers pass this to `build_agent()` to declare the trust level of
/// the session.  See `docs/public-agents.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full-featured agent: all tools, workspace, dreams.
    /// For trusted users (e.g. Telegram private chats with the operator).
    Private,
    /// Per-channel workspace agent: workspace memory + web tools only.
    /// No filesystem, shell, MCP, or subagent access.  Sandbox always enforced.
    /// For untrusted users (e.g. Telegram group chats).
    Public,
}

// ---------------------------------------------------------------------------
// ClientRegistry — one LLM client per provider, lazily created.
// ---------------------------------------------------------------------------

/// Registry of rate-limited LLM clients, one per configured provider.
///
/// Created once and shared across all controllers via `Arc`.  Clients are
/// created lazily on first access and cached.  This means rate-limit
/// windows survive provider switches and are shared across controllers —
/// switching from Claude to GPT and back doesn't reset the rate counter.
///
/// On config reload, call [`ClientRegistry::reload()`] to swap in new
/// settings.  All cached clients are dropped so subsequent `get()` calls
/// pick up new API keys / base URLs.
pub struct ClientRegistry {
    /// One `RateLimited` per provider name.  Lazily populated on first
    /// `get()` call for that provider.  Behind a `Mutex` for interior
    /// mutability so the registry can be shared via `Arc`.
    inner: std::sync::Mutex<ClientRegistryInner>,
}

struct ClientRegistryInner {
    clients: std::collections::HashMap<
        String,
        crate::agent::rate_limiter::RateLimited<Box<dyn crate::llm::LlmClient>>,
    >,
    /// Settings snapshot used to create clients on demand.
    settings: Settings,
    /// Workspace reference for CLI-subprocess providers (ClaudeCode, Codex).
    workspace: Option<crate::workspace::WorkspaceHandle>,
}

impl ClientRegistry {
    /// Create a new registry from the current settings.
    ///
    /// No clients are created yet — they are lazily instantiated on first
    /// `get()`.  The registry keeps a clone of `settings` so it can build
    /// clients at any time without borrowing from the controller.
    pub fn new(
        settings: &Settings,
        workspace: Option<crate::workspace::WorkspaceHandle>,
    ) -> Self {
        Self {
            inner: std::sync::Mutex::new(ClientRegistryInner {
                clients: std::collections::HashMap::new(),
                settings: settings.clone(),
                workspace,
            }),
        }
    }

    /// Drop all cached clients and swap in new settings.
    ///
    /// Subsequent `get()` calls will create new clients with the updated
    /// API keys / base URLs.  Call this on config reload instead of
    /// replacing the entire registry.
    pub fn reload(
        &self,
        settings: &Settings,
        workspace: Option<crate::workspace::WorkspaceHandle>,
    ) {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");
        inner.clients.clear();
        inner.settings = settings.clone();
        inner.workspace = workspace;
    }

    /// Get a `UserFacing` handle to the client for a named provider.
    ///
    /// Creates the client on first access.  Returns `Err` if the provider
    /// name is not in the settings.
    pub fn get(
        &self,
        provider_name: &str,
    ) -> crate::Result<crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>>
    {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");

        if !inner.clients.contains_key(provider_name) {
            let pc = inner.settings.providers.get(provider_name).ok_or_else(|| {
                crate::error::DysonError::Config(format!("unknown provider '{provider_name}'"))
            })?;

            let agent_settings = crate::config::AgentSettings {
                provider: pc.provider_type.clone(),
                api_key: pc.api_key.clone(),
                base_url: pc.base_url.clone(),
                ..inner.settings.agent.clone()
            };

            let client = crate::llm::create_client(
                &agent_settings,
                inner.workspace.clone(),
                inner.settings.dangerous_no_sandbox,
            );

            let rate_limited = match inner.settings.agent.rate_limit.as_ref() {
                Some(rl) => crate::agent::rate_limiter::RateLimited::new(
                    client,
                    rl.max_messages,
                    std::time::Duration::from_secs(rl.window_secs),
                ),
                None => crate::agent::rate_limiter::RateLimited::unlimited(client),
            };

            inner.clients.insert(provider_name.to_string(), rate_limited);
        }

        let rl = &inner.clients[provider_name];
        Ok(rl.handle(crate::agent::rate_limiter::Priority::UserFacing))
    }

    /// Get a handle for the default (active) provider from settings.
    ///
    /// Looks up the provider name that matches the current agent config,
    /// or falls back to creating a client directly from the agent settings.
    pub fn get_default(
        &self,
    ) -> crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>> {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");

        // Try to find the named provider that matches.
        if let Some(name) = active_provider_name(&inner.settings) {
            // Release the lock temporarily so `get()` can re-acquire it.
            drop(inner);
            if let Ok(handle) = self.get(&name) {
                return handle;
            }
            inner = self.inner.lock().expect("ClientRegistry poisoned");
        }

        // Fallback: create client from the default agent settings.
        // This handles the case where no named provider matches (e.g.
        // single-provider config without a "providers" map).
        if !inner.clients.contains_key("__default__") {
            let client = crate::llm::create_client(
                &inner.settings.agent,
                inner.workspace.clone(),
                inner.settings.dangerous_no_sandbox,
            );
            let rate_limited = match inner.settings.agent.rate_limit.as_ref() {
                Some(rl) => crate::agent::rate_limiter::RateLimited::new(
                    client,
                    rl.max_messages,
                    std::time::Duration::from_secs(rl.window_secs),
                ),
                None => crate::agent::rate_limiter::RateLimited::unlimited(client),
            };
            inner.clients.insert("__default__".to_string(), rate_limited);
        }

        inner.clients["__default__"]
            .handle(crate::agent::rate_limiter::Priority::UserFacing)
    }
}

/// Build an agent from settings.
///
/// `mode` controls the trust level — `AgentMode::Private` builds a
/// full-featured agent, `AgentMode::Public` builds a hardened agent with
/// per-channel workspace memory and web tools.  See `docs/public-agents.md`.
///
/// `channel_id` is required for `AgentMode::Public` — it determines which
/// per-channel workspace subdirectory to use.
///
/// The `client` handle comes from a [`ClientRegistry`] — all agents
/// share the same LLM client and rate-limit window per provider.
///
/// Every controller should use this instead of building agents manually.
/// The mode is the single point of control — individual controllers just
/// declare the trust level, and this function handles the rest.
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    mode: AgentMode,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    registry: &ClientRegistry,
    channel_id: Option<&str>,
) -> crate::Result<crate::agent::Agent> {
    if mode == AgentMode::Public {
        let ch = channel_id.unwrap_or("unknown");
        return build_public_agent(settings, controller_prompt, client, ch);
    }

    // --- Private agent: full tools, workspace, dreams ---

    let workspace = crate::workspace::create_workspace(&settings.workspace)?;

    let mut agent_settings = settings.agent.clone();

    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    if let Some(prompt) = controller_prompt {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(prompt);
    }

    let workspace: crate::workspace::WorkspaceHandle =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    let nudge_interval = {
        let ws = workspace.read().await;
        ws.nudge_interval()
    };

    let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, settings.dangerous_no_sandbox);
    let skills = {
        let ws = workspace.read().await;
        crate::skill::create_skills(
            settings,
            Some(&**ws),
            std::sync::Arc::clone(&sandbox),
            Some(std::sync::Arc::clone(&workspace)),
            registry,
        )
        .await
    };

    let transcriber = crate::media::audio::create_transcriber(settings.transcriber.as_ref());

    let mut builder = crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .transcriber(transcriber);

    // Create advisor if smartest_model is configured.
    // Format: "provider_name/model" (e.g. "openrouter/glm-5", "claude/claude-opus-4-6").
    // Skip if the advisor resolves to the same model the executor is already using.
    if let Some(ref smartest_model) = settings.agent.smartest_model {
        if let Some((provider_name, advisor_model)) = smartest_model.split_once('/') {
            // Skip if the advisor is the currently loaded model.
            let is_same_model = settings
                .providers
                .get(provider_name)
                .is_some_and(|pc| {
                    pc.provider_type == settings.agent.provider
                        && advisor_model == settings.agent.model
                });

            if !is_same_model {
                let advisor_provider_type = settings
                    .providers
                    .get(provider_name)
                    .map(|pc| pc.provider_type.clone())
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            provider = provider_name,
                            "advisor provider not found, falling back to generic"
                        );
                        crate::config::LlmProvider::OpenAi // will use generic path
                    });

                let advisor_client = match registry.get(provider_name) {
                    Ok(handle) => handle,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to create advisor client, skipping");
                        return builder.build();
                    }
                };

                let advisor = crate::advisor::create_advisor(
                    &settings.agent.provider,
                    &advisor_provider_type,
                    advisor_model,
                    advisor_client,
                );
                builder = builder.advisor(advisor);
            } else {
                tracing::info!(
                    smartest_model = smartest_model.as_str(),
                    "advisor model is the currently loaded model, skipping"
                );
            }
        } else {
            tracing::warn!(
                smartest_model = smartest_model.as_str(),
                "smartest_model must be in 'provider/model' format (e.g. 'claude/claude-opus-4-6')"
            );
        }
    }

    builder.build()
}

/// Tools available to public agents — workspace memory + web research.
const PUBLIC_AGENT_TOOLS: &[&str] = &[
    "workspace",
    "memory_search",
    "web_fetch",
    "web_search",
];

/// Build a public agent with a per-channel workspace.
///
/// The agent gets its own workspace under `{main_workspace}/channels/{channel_id}/`,
/// with SOUL.md and IDENTITY.md symlinked from the main workspace (read-only).
/// Tools are restricted to workspace memory operations and web research.
/// Sandbox is always enforced.
fn build_public_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    channel_id: &str,
) -> crate::Result<crate::agent::Agent> {
    let filter: Vec<String> = PUBLIC_AGENT_TOOLS.iter().map(|s| (*s).to_string()).collect();
    let ig_provider = settings
        .agent
        .image_generation_provider
        .as_ref()
        .and_then(|name| settings.providers.get(name));
    let skills: Vec<Box<dyn crate::skill::Skill>> = vec![Box::new(
        crate::skill::builtin::BuiltinSkill::new_filtered(
            settings.web_search.as_ref(),
            ig_provider,
            settings.agent.image_generation_model.as_deref(),
            &filter,
        ),
    )];

    let workspace = crate::workspace::create_channel_workspace(
        &settings.workspace,
        channel_id,
    )?;

    let mut agent_settings = settings.agent.clone();

    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    agent_settings.system_prompt.push_str(
        "\n\nYou are a public-facing agent. You can search the web, fetch web pages, \
         and maintain persistent memory for this channel. You do NOT have access \
         to the filesystem, shell commands, or the operator's private workspace. \
         Be concise and cite your sources.",
    );

    if let Some(prompt) = controller_prompt {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(prompt);
    }

    let nudge_interval = workspace.nudge_interval();

    let workspace: crate::workspace::WorkspaceHandle =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    // SECURITY: Always false — public agent sandbox is never disabled.
    let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, false);

    crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .build()
}

/// Parse a `/model` command argument into (provider_name, optional_model).
///
/// Resolution order:
/// 1. If the first word matches a provider name, use it as provider.
///    A second word (if present) is the model within that provider.
/// 2. Otherwise, check if the entire argument is a model in the current provider.
/// 3. If neither matches, return an error.
fn parse_model_command(
    args: &str,
    providers: &std::collections::HashMap<String, crate::config::ProviderConfig>,
    current_provider: &str,
) -> Result<(String, Option<String>), String> {
    let args = args.trim();
    if args.is_empty() {
        return Err("Usage: /model <provider> [model]  or  /model <model>".to_string());
    }

    // Split into at most 2 parts: potential provider + potential model.
    let mut parts = args.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap(); // always present after empty check
    let second = parts.next().map(str::trim);

    // Case 1: first word is a known provider name.
    if providers.contains_key(first) {
        return Ok((first.to_string(), second.map(std::string::ToString::to_string)));
    }

    // Case 2: not a provider — try as a model in the current provider.
    if let Some(pc) = providers.get(current_provider)
        && pc.models.iter().any(|m| m == args)
    {
        return Ok((current_provider.to_string(), Some(args.to_string())));
    }

    Err(format!("unknown provider or model '{first}'"))
}

/// Find the provider name in `settings.providers` that matches the active
/// agent config (provider type + model).  Returns `None` if no match.
pub fn active_provider_name(settings: &Settings) -> Option<String> {
    settings.providers.iter().find_map(|(name, pc)| {
        if pc.provider_type == settings.agent.provider && pc.models.contains(&settings.agent.model)
        {
            Some(name.clone())
        } else {
            None
        }
    })
}

/// List all configured providers, sorted by name.
pub fn list_providers(settings: &Settings) -> Vec<(&str, &crate::config::ProviderConfig)> {
    let mut providers: Vec<_> = settings
        .providers
        .iter()
        .map(|(name, config)| (name.as_str(), config))
        .collect();
    providers.sort_by_key(|(name, _)| *name);
    providers
}

// ---------------------------------------------------------------------------
// Hot-reload setup — shared across all controllers.
// ---------------------------------------------------------------------------

/// Resolve the config file path and create a hot reloader.
///
/// Both terminal and Telegram controllers need to watch for config and
/// workspace changes.  This extracts the shared setup:
/// 1. Parse `--config` / `-c` from CLI args, or fall back to `dyson.json`
/// 2. Resolve the workspace path
/// 3. Create a `HotReloader` watching both
pub fn create_hot_reloader(
    settings: &Settings,
) -> (Option<PathBuf>, crate::config::hot_reload::HotReloader) {
    let config_path = std::env::args()
        .skip_while(|a| a != "--config" && a != "-c")
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from("dyson.json");
            if p.exists() { Some(p) } else { None }
        });
    let workspace_path = crate::workspace::FilesystemWorkspace::resolve_path(Some(
        settings.workspace.connection_string.expose(),
    ));
    let reloader = crate::config::hot_reload::HotReloader::new(
        config_path.as_deref(),
        workspace_path.as_deref(),
    );
    (config_path, reloader)
}

// ---------------------------------------------------------------------------
// Single-agent reload — used by terminal controller.
// ---------------------------------------------------------------------------

/// Outcome of a hot-reload check.
pub enum ReloadOutcome {
    /// Nothing changed.
    NoChange,
    /// Agent was rebuilt (config or workspace changed).
    Reloaded,
    /// Reload check or rebuild failed.
    Error(String),
}

/// Check for config/workspace changes and rebuild the agent if needed.
///
/// Preserves the user's provider/model selection across reloads.  Falls
/// back to defaults only if the selected provider/model was removed from
/// the new config.
#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
pub async fn check_and_reload_agent(
    reloader: &mut crate::config::hot_reload::HotReloader,
    current_settings: &mut Settings,
    original_dangerous_no_sandbox: bool,
    agent: &mut crate::agent::Agent,
    current_provider: &mut String,
    current_model: &mut String,
    controller_prompt: Option<&str>,
    registry: &ClientRegistry,
) -> ReloadOutcome {
    let (changed, new_settings) = match reloader.check().await {
        Ok(result) => result,
        Err(e) => return ReloadOutcome::Error(format!("config reload check failed: {e}")),
    };

    if !changed {
        return ReloadOutcome::NoChange;
    }

    if let Some(s) = new_settings {
        *current_settings = s;
        current_settings.dangerous_no_sandbox = original_dangerous_no_sandbox;
    }

    // Reload the client registry so new API keys / base URLs take effect.
    registry.reload(current_settings, None);

    let messages = agent.messages().to_vec();
    let client = registry.get_default();
    match build_agent(current_settings, controller_prompt, AgentMode::Private, client, registry, None).await {
        Ok(mut a) => {
            a.set_messages(messages);
            // Restore the user's provider/model selection if it differs
            // from the default.
            if let Some(pc) = current_settings.providers.get(current_provider.as_str())
                && let Ok(handle) = registry.get(current_provider)
            {
                a.swap_client(handle, current_model, &pc.provider_type);
            }
            *agent = a;
        }
        Err(e) => {
            return ReloadOutcome::Error(format!("reload error: {e}"));
        }
    }

    ReloadOutcome::Reloaded
}

// ---------------------------------------------------------------------------
// Shared command dispatch
// ---------------------------------------------------------------------------

/// A provider and its models, ready for rendering by controllers.
pub struct ProviderInfo {
    pub name: String,
    pub provider_type: String,
    pub models: Vec<ModelInfo>,
}

/// A single model within a provider.
pub struct ModelInfo {
    pub name: String,
    pub active: bool,
}

/// Result of executing a shared command.
///
/// Controllers match on this to render output and add controller-specific
/// side effects (e.g. Telegram persists chat history after compaction).
pub enum CommandResult {
    /// `/clear` succeeded — agent context was cleared.
    Cleared,
    /// `/compact` succeeded — conversation was compacted.
    Compacted,
    /// `/compact` failed.
    CompactError(String),
    /// `/models` — list of providers and their models.
    ModelList { providers: Vec<ProviderInfo> },
    /// `/model` succeeded — switched to a new provider/model.
    ModelSwitched {
        provider_name: String,
        provider_type: String,
        model: String,
    },
    /// `/model` failed — could not switch.
    ModelSwitchError(String),
    /// `/model` with bad arguments.
    ModelParseError(String),
    /// `/model` with no arguments — show usage.
    ModelUsage,
    /// `/logs` — recent log lines.
    Logs(String),
    /// `/logs` failed — could not read log file.
    LogsError(String),
    /// `/loop` succeeded — background agent spawned.
    LoopStarted {
        id: u64,
        prompt_preview: String,
        chat_id: String,
    },
    /// `/loop` failed.
    LoopError(String),
    /// `/agents` — list of running background agents.
    AgentList {
        agents: Vec<background::BackgroundAgentListEntry>,
    },
    /// `/stop` succeeded — agent cancellation requested.
    AgentStopped { id: u64 },
    /// `/stop` failed — invalid ID or agent not found.
    StopError(String),
    /// Input was not a shared command — controller should handle it.
    NotHandled,
}

/// Execute a lock-free command that doesn't need the agent.
///
/// Handles: `/logs`, `/agents`, `/loop`, `/stop`, `/models`.
/// Returns `NotHandled` for commands that require the agent lock.
/// Callback invoked when a background agent finishes.
///
/// Receives the agent ID and either the final response text (`Ok`) or a
/// human-readable failure message (`Err`).  Controllers use this to surface
/// background agent results to the user who spawned them.
pub type BackgroundCompletion =
    std::sync::Arc<dyn Fn(u64, Result<String, String>) + Send + Sync>;

pub async fn execute_lockfree_command(
    input: &str,
    settings: &Settings,
    registry: &ClientRegistry,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: Option<BackgroundCompletion>,
) -> CommandResult {
    if input == "/logs" || input.starts_with("/logs ") {
        let n: usize = input
            .strip_prefix("/logs")
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(20);
        return match tokio::task::spawn_blocking(move || read_log_tail(n)).await {
            Ok(Ok(lines)) => CommandResult::Logs(lines),
            Ok(Err(e)) => CommandResult::LogsError(e),
            Err(e) => CommandResult::LogsError(format!("task failed: {e}")),
        };
    }

    if input == "/models" {
        if settings.providers.is_empty() {
            return CommandResult::ModelList {
                providers: Vec::new(),
            };
        }
        // Note: active-model highlighting requires provider/model state that
        // lives in the agent.  For the lock-free path we omit it — /models
        // is informational and the inline keyboard handles selection.
        let providers = list_providers(settings)
            .into_iter()
            .map(|(name, pc)| ProviderInfo {
                name: name.to_string(),
                provider_type: format!("{:?}", pc.provider_type),
                models: pc
                    .models
                    .iter()
                    .map(|m| ModelInfo {
                        name: m.clone(),
                        active: false,
                    })
                    .collect(),
            })
            .collect();
        return CommandResult::ModelList { providers };
    }

    if input == "/agents" {
        return CommandResult::AgentList {
            agents: bg_registry.list(),
        };
    }

    if input == "/loop" {
        return CommandResult::LoopError("usage: /loop <prompt>".to_string());
    }
    if let Some(args) = input.strip_prefix("/loop ").map(str::trim) {
        if args.is_empty() {
            return CommandResult::LoopError("usage: /loop <prompt>".to_string());
        }
        return spawn_background_agent(args, settings, registry, bg_registry, on_complete).await;
    }

    if input == "/stop" {
        return CommandResult::StopError("usage: /stop <id>".to_string());
    }
    if let Some(args) = input.strip_prefix("/stop ").map(str::trim) {
        let id: u64 = match args.parse() {
            Ok(id) => id,
            Err(_) => return CommandResult::StopError("invalid agent ID".to_string()),
        };
        return match bg_registry.stop(id) {
            Ok(()) => CommandResult::AgentStopped { id },
            Err(e) => CommandResult::StopError(e),
        };
    }

    CommandResult::NotHandled
}

/// Mutable model-switching state threaded through agent commands.
pub struct ModelState<'a> {
    pub provider: &'a mut String,
    pub model: &'a mut String,
    pub config_path: Option<&'a Path>,
}

/// Execute a command that requires the agent lock.
///
/// Handles: `/clear`, `/compact`, `/model`.
/// Returns `NotHandled` for everything else.
pub async fn execute_agent_command(
    input: &str,
    agent: &mut crate::agent::Agent,
    output: &mut dyn Output,
    settings: &Settings,
    ms: &mut ModelState<'_>,
    registry: &ClientRegistry,
) -> CommandResult {
    if input == "/clear" {
        agent.clear();
        return CommandResult::Cleared;
    }

    if input == "/compact" {
        return match agent.compact(output).await {
            Ok(()) => CommandResult::Compacted,
            Err(e) => CommandResult::CompactError(e.to_string()),
        };
    }

    if input == "/model" {
        return CommandResult::ModelUsage;
    }

    if let Some(args) = input.strip_prefix("/model ").map(str::trim) {
        if args.is_empty() {
            return CommandResult::ModelUsage;
        }
        let (target_provider, target_model) = match parse_model_command(
            args,
            &settings.providers,
            ms.provider,
        ) {
            Ok(parsed) => parsed,
            Err(e) => return CommandResult::ModelParseError(e),
        };
        let pc = match settings.providers.get(&target_provider) {
            Some(pc) => pc,
            None => {
                return CommandResult::ModelSwitchError(format!(
                    "unknown provider '{target_provider}'"
                ))
            }
        };
        let resolved = target_model
            .as_deref()
            .unwrap_or_else(|| pc.default_model())
            .to_string();
        match registry.get(&target_provider) {
            Ok(handle) => {
                agent.swap_client(handle, &resolved, &pc.provider_type);
                *ms.model = resolved.clone();
                *ms.provider = target_provider.clone();
                if let Some(cp) = ms.config_path {
                    crate::config::loader::persist_model_selection(
                        cp,
                        &target_provider,
                        &resolved,
                    );
                }
                return CommandResult::ModelSwitched {
                    provider_name: target_provider,
                    provider_type: format!("{:?}", pc.provider_type),
                    model: resolved,
                };
            }
            Err(e) => return CommandResult::ModelSwitchError(e.to_string()),
        }
    }

    CommandResult::NotHandled
}

/// Spawn a background agent with unlimited iterations.
///
/// Constructs a fresh agent from the current settings, wires up a
/// `CancellationToken` for `/stop`, and runs it in a `tokio::spawn` task.
/// The agent's conversation is persisted through the existing chat history
/// system under chat ID `bg-<id>`, reusing the same storage and media
/// externalization as regular conversations.
/// Finalise a background agent run: invoke the completion callback (if any)
/// and remove the registry entry.  Extracted so it can be unit-tested without
/// spinning up a real agent / LLM client.
pub(crate) fn finish_background_agent(
    id: u64,
    result: Result<String, String>,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: &Option<BackgroundCompletion>,
) {
    if let Some(cb) = on_complete.as_ref() {
        cb(id, result);
    }
    bg_registry.remove(id);
}

/// Render a background agent's final outcome as transcript text.
///
/// Shared by the Telegram notification path and by the chat-history append
/// so the user sees identical wording in both places.
pub fn format_background_result(id: u64, result: &std::result::Result<String, String>) -> String {
    match result {
        Ok(text) if !text.trim().is_empty() => format!("Background agent #{id} result:\n{text}"),
        Ok(_) => format!("Background agent #{id} finished with no response."),
        Err(e) => format!("Background agent #{id} failed: {e}"),
    }
}

/// Append a background agent's result to a chat's persisted history.
///
/// Loads the stored conversation, pushes a single user message containing
/// the formatted outcome, and writes it back.  Returns the full updated
/// vector so callers with a live in-memory agent can refresh its state in
/// lock-step with the on-disk copy.
pub fn persist_background_result(
    store: &dyn crate::chat_history::ChatHistory,
    chat_key: &str,
    id: u64,
    result: &std::result::Result<String, String>,
) -> crate::error::Result<Vec<crate::message::Message>> {
    let mut msgs = store.load(chat_key)?;
    msgs.push(crate::message::Message::user(&format_background_result(
        id, result,
    )));
    store.save(chat_key, &msgs)?;
    Ok(msgs)
}

pub(crate) async fn spawn_background_agent(
    prompt: &str,
    settings: &Settings,
    registry: &ClientRegistry,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: Option<BackgroundCompletion>,
) -> CommandResult {
    use crate::agent::rate_limiter::Priority;
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();

    let prompt_preview = if prompt.len() > 100 {
        format!("{}...", &prompt[..97])
    } else {
        prompt.to_string()
    };

    let id = match bg_registry.allocate(prompt_preview.clone(), cancel.clone()) {
        Ok(id) => id,
        Err(e) => return CommandResult::LoopError(e),
    };

    let chat_id = format!("bg-{id}");

    // Build the background agent with unlimited iterations and Background priority.
    let bg_client = registry.get_default().with_priority(Priority::Background);

    let mut bg_settings = settings.clone();
    bg_settings.agent.max_iterations = usize::MAX;

    let mut bg_agent = match build_agent(
        &bg_settings,
        None,
        AgentMode::Private,
        bg_client,
        registry,
        None,
    )
    .await
    {
        Ok(agent) => agent,
        Err(e) => {
            bg_registry.remove(id);
            return CommandResult::LoopError(format!("failed to build agent: {e}"));
        }
    };

    bg_agent.set_cancellation_token(cancel);

    // Attach chat history so the conversation is persisted through the
    // existing chat store (same backend as Telegram / other controllers).
    if let Ok(store) = crate::chat_history::create_chat_history(&settings.chat_history) {
        let store: std::sync::Arc<dyn crate::chat_history::ChatHistory> = std::sync::Arc::from(store);
        bg_agent.set_chat_history(store, chat_id.clone());
    }

    let prompt_owned = prompt.to_string();
    let bg_reg = std::sync::Arc::clone(bg_registry);
    let mut output = crate::agent::SilentOutput;

    let handle = tokio::spawn(async move {
        tracing::info!(id, prompt = %prompt_owned, "background agent starting");
        let result: Result<String, String> = match bg_agent.run(&prompt_owned, &mut output).await {
            Ok(text) => {
                tracing::info!(
                    id,
                    text_len = text.len(),
                    "background agent completed"
                );
                Ok(text)
            }
            Err(e) => {
                tracing::warn!(
                    id,
                    error = %e,
                    "background agent failed"
                );
                Err(e.to_string())
            }
        };
        finish_background_agent(id, result, &bg_reg, &on_complete);
    });

    bg_registry.set_handle(id, handle);

    CommandResult::LoopStarted {
        id,
        prompt_preview,
        chat_id,
    }
}

/// Read the last `n` lines from the most recent `~/.dyson/dyson.log.*` file.
///
/// `tracing_appender::rolling::daily` creates files like `dyson.log.2026-04-03`.
/// We pick the most recent one by sorting the matching filenames.
pub fn read_log_tail(n: usize) -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let log_dir = PathBuf::from(home).join(".dyson");
    read_log_tail_from_dir(&log_dir, n)
}

/// Read the last `n` lines from the most recent `dyson.log*` file in `log_dir`.
fn read_log_tail_from_dir(log_dir: &std::path::Path, n: usize) -> Result<String, String> {
    use std::io::{Read as _, Seek, SeekFrom};

    let mut log_files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .map_err(|e| format!("cannot read {}: {e}", log_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("dyson.log") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();

    if log_files.is_empty() {
        return Err("no log files found".to_string());
    }

    // Sort descending so the most recent date-suffixed file comes first.
    log_files.sort();
    log_files.reverse();

    let path = &log_files[0];
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    // Read from the end of the file in chunks to find the last `n` lines,
    // avoiding loading the entire file into memory.
    let file_len = file
        .metadata()
        .map_err(|e| e.to_string())?
        .len();

    if file_len == 0 {
        return Ok(String::new());
    }

    const CHUNK: u64 = 8192;
    // If the file does not end with '\n', the content after the last newline
    // is already one line, so start the count at 1.
    let mut newlines_found = {
        file.seek(SeekFrom::End(-1)).map_err(|e| e.to_string())?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last).map_err(|e| e.to_string())?;
        if last[0] == b'\n' { 0usize } else { 1usize }
    };
    let mut tail_start = 0u64; // byte offset where the tail begins
    let mut offset = file_len;

    // Walk backwards through the file one chunk at a time.
    'outer: while offset > 0 {
        let read_start = offset.saturating_sub(CHUNK);
        let read_len = (offset - read_start) as usize;
        file.seek(SeekFrom::Start(read_start)).map_err(|e| e.to_string())?;

        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf).map_err(|e| e.to_string())?;

        // Scan the chunk from back to front for newline characters.
        for i in (0..read_len).rev() {
            if buf[i] == b'\n' {
                newlines_found += 1;
                // We need n+1 newlines to capture n complete lines (the last
                // newline may be at EOF, so the +1 accounts for that).
                if newlines_found > n {
                    tail_start = read_start + (i as u64) + 1;
                    break 'outer;
                }
            }
        }

        offset = read_start;
    }

    // Read from tail_start to end of file.
    file.seek(SeekFrom::Start(tail_start)).map_err(|e| e.to_string())?;
    let mut result = String::new();
    file.read_to_string(&mut result).map_err(|e| e.to_string())?;

    // Trim a single trailing newline so the caller gets clean lines.
    if result.ends_with('\n') {
        result.pop();
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Output trait
// ---------------------------------------------------------------------------

/// Rendering interface for agent events.
///
/// The agent loop calls these methods as events occur.  Each controller
/// creates an appropriate Output implementation (e.g. writing to stdout,
/// editing chat messages, streaming over HTTP).
///
/// ## Why separate from Controller?
///
/// The agent needs a render target (`&mut dyn Output`) but doesn't know
/// about input sourcing, lifecycle, or access control — those are the
/// controller's job.  Separating Output keeps the agent loop clean.
///
/// ```text
/// Controller (owns lifecycle)
///   │
///   ├── creates Output per session/message
///   │     │
///   │     ▼
///   └── agent.run(input, &mut output)
///         │
///         ├── output.text_delta("Hello")
///         ├── output.tool_use_start(...)
///         ├── output.tool_result(...)
///         └── output.flush()
/// ```
pub trait Output: Send {
    /// A fragment of text from the LLM's response.
    fn text_delta(&mut self, text: &str) -> std::result::Result<(), DysonError>;

    /// The LLM is starting a tool call.
    fn tool_use_start(&mut self, id: &str, name: &str) -> std::result::Result<(), DysonError>;

    /// The tool call definition is complete (input JSON fully received).
    fn tool_use_complete(&mut self) -> std::result::Result<(), DysonError>;

    /// A tool has finished executing.
    fn tool_result(&mut self, output: &ToolOutput) -> std::result::Result<(), DysonError>;

    /// Send a file to the user.
    ///
    /// Called by the agent loop when a tool attaches files to its output.
    /// The file is delivered as a side-channel to the user — it does not
    /// appear in the LLM's conversation history.
    ///
    /// Each controller delivers files differently (e.g. printing the path,
    /// sending a document message).
    fn send_file(&mut self, path: &Path) -> std::result::Result<(), DysonError>;

    /// Receive a progress checkpoint event emitted by a tool call.
    ///
    /// Called by the agent loop whenever a tool attaches one or more
    /// `CheckpointEvent`s to its output.  Like `send_file`, this is a
    /// side-channel — the event does not appear in the LLM's conversation
    /// history.
    ///
    /// The default impl drops the event, which is the correct behaviour
    /// for every controller except `SwarmController`.  The swarm
    /// controller forwards checkpoints to the hub via
    /// `POST /swarm/checkpoint` so callers observing a long-running
    /// task see progress in real time.
    fn checkpoint(
        &mut self,
        event: &CheckpointEvent,
    ) -> std::result::Result<(), DysonError> {
        let _ = event;
        Ok(())
    }

    /// An error occurred.
    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError>;

    /// Called when the LLM returns a non-retryable error during the agent loop.
    ///
    /// The controller inspects the error and returns a [`LlmRecovery`] action
    /// telling the agent loop how to proceed.  The default implementation
    /// returns [`LlmRecovery::GiveUp`], which propagates the error to the
    /// caller unchanged.
    ///
    /// Controllers may use this hook to send user-facing messages (e.g.
    /// "model doesn't support tools") before returning a recovery action.
    fn on_llm_error(&mut self, error: &DysonError) -> crate::error::LlmRecovery {
        let _ = error;
        crate::error::LlmRecovery::GiveUp
    }

    /// Show or hide a typing indicator.
    ///
    /// Called with `visible = true` just before the LLM call starts (after
    /// the user sends input) and `visible = false` once the first response
    /// token arrives.  Controllers that support a typing indicator should
    /// display/clear it accordingly.
    ///
    /// The default implementation is a no-op.
    fn typing_indicator(&mut self, _visible: bool) -> std::result::Result<(), DysonError> {
        Ok(())
    }

    /// Flush any buffered output.
    fn flush(&mut self) -> std::result::Result<(), DysonError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;
    use std::io::Write;

    type BgReceived = std::sync::Arc<std::sync::Mutex<Option<(u64, Result<String, String>)>>>;

    fn make_log_dir(content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("dyson.log.2026-04-03");
        let mut f = std::fs::File::create(log_path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn tail_basic() {
        let dir = make_log_dir("line1\nline2\nline3\nline4\nline5\n");
        let result = read_log_tail_from_dir(dir.path(), 3).unwrap();
        assert_eq!(result, "line3\nline4\nline5");
    }

    #[test]
    fn tail_more_than_available() {
        let dir = make_log_dir("line1\nline2\n");
        let result = read_log_tail_from_dir(dir.path(), 10).unwrap();
        assert_eq!(result, "line1\nline2");
    }

    #[test]
    fn tail_exact_count() {
        let dir = make_log_dir("line1\nline2\nline3\n");
        let result = read_log_tail_from_dir(dir.path(), 3).unwrap();
        assert_eq!(result, "line1\nline2\nline3");
    }

    #[test]
    fn tail_single_line() {
        let dir = make_log_dir("only\n");
        let result = read_log_tail_from_dir(dir.path(), 1).unwrap();
        assert_eq!(result, "only");
    }

    #[test]
    fn tail_no_trailing_newline() {
        let dir = make_log_dir("line1\nline2\nline3");
        let result = read_log_tail_from_dir(dir.path(), 2).unwrap();
        assert_eq!(result, "line2\nline3");
    }

    #[test]
    fn background_agent_delivers_result_to_callback() {
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
        let id = bg_reg
            .allocate("test".into(), CancellationToken::new())
            .unwrap();

        let received: BgReceived = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);
        let cb: BackgroundCompletion = Arc::new(move |id, r| {
            *received_clone.lock().unwrap() = Some((id, r));
        });

        finish_background_agent(id, Ok("hello".into()), &bg_reg, &Some(cb));

        let got = received.lock().unwrap().clone();
        assert_eq!(got, Some((id, Ok("hello".to_string()))));
        assert!(bg_reg.list().is_empty(), "registry entry removed");
    }

    #[test]
    fn background_agent_delivers_error_to_callback() {
        use std::sync::{Arc, Mutex};
        use tokio_util::sync::CancellationToken;

        let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
        let id = bg_reg
            .allocate("test".into(), CancellationToken::new())
            .unwrap();

        let received: BgReceived = Arc::new(Mutex::new(None));
        let received_clone = Arc::clone(&received);
        let cb: BackgroundCompletion = Arc::new(move |id, r| {
            *received_clone.lock().unwrap() = Some((id, r));
        });

        finish_background_agent(id, Err("boom".into()), &bg_reg, &Some(cb));

        let got = received.lock().unwrap().clone();
        assert_eq!(got, Some((id, Err("boom".to_string()))));
        assert!(bg_reg.list().is_empty());
    }

    #[test]
    fn format_background_result_variants() {
        assert_eq!(
            format_background_result(7, &Ok("done".into())),
            "Background agent #7 result:\ndone",
        );
        assert_eq!(
            format_background_result(7, &Ok("   ".into())),
            "Background agent #7 finished with no response.",
        );
        assert_eq!(
            format_background_result(9, &Err("boom".into())),
            "Background agent #9 failed: boom",
        );
    }

    #[test]
    fn persist_background_result_appends_to_chat_history() {
        use crate::chat_history::{ChatHistory, DiskChatHistory};
        use crate::message::{ContentBlock, Message, Role};

        let dir = tempfile::tempdir().unwrap();
        let store = DiskChatHistory::new(dir.path().to_path_buf()).unwrap();
        let chat_key = "123";
        store.save(chat_key, &[Message::user("hi")]).unwrap();

        let returned =
            persist_background_result(&store, chat_key, 7, &Ok("result text".into())).unwrap();
        assert_eq!(returned.len(), 2);

        let loaded = store.load(chat_key).unwrap();
        assert_eq!(loaded.len(), 2, "original turn + appended result");
        let last = loaded.last().unwrap();
        assert_eq!(last.role, Role::User);
        match &last.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Background agent #7"), "got: {text}");
                assert!(text.contains("result text"), "got: {text}");
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn background_agent_finish_without_callback_still_prunes() {
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
        let id = bg_reg
            .allocate("test".into(), CancellationToken::new())
            .unwrap();

        finish_background_agent(id, Ok("x".into()), &bg_reg, &None);
        assert!(bg_reg.list().is_empty());
    }

    #[test]
    fn tail_empty_file() {
        let dir = make_log_dir("");
        let result = read_log_tail_from_dir(dir.path(), 5).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn tail_picks_most_recent_file() {
        let dir = tempfile::tempdir().unwrap();
        // Older file
        std::fs::write(dir.path().join("dyson.log.2026-04-01"), "old\n").unwrap();
        // Newer file
        std::fs::write(dir.path().join("dyson.log.2026-04-03"), "new\n").unwrap();
        let result = read_log_tail_from_dir(dir.path(), 1).unwrap();
        assert_eq!(result, "new");
    }

    #[test]
    fn tail_no_log_files() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_log_tail_from_dir(dir.path(), 5);
        assert!(result.is_err());
    }

    #[test]
    fn tail_large_file_spanning_chunks() {
        // Create a file larger than the 8192-byte chunk size.
        use std::fmt::Write as _;
        let mut content = String::new();
        for i in 0..500 {
            writeln!(&mut content, "log line number {i:04}").unwrap();
        }
        let dir = make_log_dir(&content);
        let result = read_log_tail_from_dir(dir.path(), 5).unwrap();
        let lines: Vec<&str> = result.split('\n').collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "log line number 0495");
        assert_eq!(lines[4], "log line number 0499");
    }

    // -----------------------------------------------------------------------
    // /logs N integration: parsing + read_log_tail end-to-end
    // -----------------------------------------------------------------------

    #[test]
    fn logs_command_with_line_count() {
        let dir = make_log_dir("line1\nline2\nline3\nline4\nline5\n");

        let input = "/logs 3";
        let n: usize = input
            .strip_prefix("/logs")
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(20);
        assert_eq!(n, 3);

        let result = read_log_tail_from_dir(dir.path(), n).unwrap();
        assert_eq!(result, "line3\nline4\nline5");
    }

    #[test]
    fn logs_command_default_count() {
        let input = "/logs";
        let n: usize = input
            .strip_prefix("/logs")
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(20);
        assert_eq!(n, 20);
    }

    // -----------------------------------------------------------------------
    // Public agent tool and read-only tests
    //
    // These verify that public agents get the correct tool set (workspace
    // memory + web, no filesystem/shell) and that identity files are
    // protected from writes via workspace.is_read_only().
    // -----------------------------------------------------------------------

    #[test]
    fn public_agent_has_workspace_and_web_tools() {
        // Build an agent using the same skill filter as build_public_agent.
        let filter: Vec<String> = PUBLIC_AGENT_TOOLS.iter().map(|s| (*s).to_string()).collect();
        let skills: Vec<Box<dyn crate::skill::Skill>> = vec![Box::new(
            crate::skill::builtin::BuiltinSkill::new_filtered(None, None, None, &filter),
        )];

        let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
            std::sync::Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
        let client = crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            false,
        );
        let client = crate::agent::rate_limiter::RateLimitedHandle::unlimited(client);

        let agent = crate::agent::Agent::builder(client, sandbox)
            .skills(skills)
            .settings(&crate::config::AgentSettings::default())
            .build()
            .unwrap();

        // Public agent SHOULD have workspace memory + web tools.
        assert!(agent.has_tool("workspace"), "must have workspace");
        assert!(agent.has_tool("memory_search"), "must have memory_search");
        assert!(agent.has_tool("web_fetch"), "must have web_fetch");
        // web_search is conditional on config, so not tested here.

        // Public agent should NOT have filesystem/shell tools.
        assert!(!agent.has_tool("bash"), "must not have bash");
        assert!(!agent.has_tool("read_file"), "must not have read_file");
        assert!(!agent.has_tool("write_file"), "must not have write_file");
        assert!(!agent.has_tool("edit_file"), "must not have edit_file");
        assert!(!agent.has_tool("list_files"), "must not have list_files");
        assert!(!agent.has_tool("search_files"), "must not have search_files");
        assert!(!agent.has_tool("send_file"), "must not have send_file");
        assert!(!agent.has_tool("load_skill"), "must not have load_skill");
        assert!(!agent.has_tool("kb_search"), "must not have kb_search");
        assert!(!agent.has_tool("kb_status"), "must not have kb_status");
    }

    #[tokio::test]
    async fn channel_workspace_only_allows_whitelisted_writes() {
        use crate::workspace::{InMemoryWorkspace, channel::ChannelWorkspace};

        let inner = InMemoryWorkspace::new()
            .with_file("SOUL.md", "Be helpful.")
            .with_file("IDENTITY.md", "I am a test bot.")
            .with_file("MEMORY.md", "");

        let ws = ChannelWorkspace::new(Box::new(inner))
            .allow("MEMORY.md")
            .allow("USER.md")
            .allow_prefix("memory/");

        let ctx = crate::tool::ToolContext::for_test_with_workspace(ws);
        let tool = crate::tool::workspace::WorkspaceTool;

        // Writing to SOUL.md — not whitelisted, silently dropped.
        let _ = tool
            .run(
                &serde_json::json!({
                    "op": "update",
                    "file": "SOUL.md",
                    "content": "Be evil.",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let ws = ctx.workspace("test").unwrap().read().await;
        assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
        drop(ws);

        // Writing to MEMORY.md — whitelisted, succeeds.
        let _ = tool
            .run(
                &serde_json::json!({
                    "op": "update",
                    "file": "MEMORY.md",
                    "content": "Learned something.",
                    "mode": "set"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let ws = ctx.workspace("test").unwrap().read().await;
        assert_eq!(ws.get("MEMORY.md").unwrap(), "Learned something.");
    }

    #[test]
    fn public_agent_tools_constant_matches_expected() {
        let expected = &["workspace", "memory_search", "web_fetch", "web_search"];
        assert_eq!(PUBLIC_AGENT_TOOLS, expected);
    }

}
