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

pub mod recording;
pub mod telegram;
pub mod terminal;

use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::error::DysonError;
use crate::tool::ToolOutput;

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
    async fn run(&self, settings: &Settings) -> crate::Result<()>;

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
    /// Hardened agent: web_search + web_fetch only, no filesystem/shell/workspace.
    /// Sandbox always enforced.  For untrusted users (e.g. Telegram group chats).
    Public,
}

/// Build an agent from settings.
///
/// `mode` controls the trust level — `AgentMode::Private` builds a
/// full-featured agent, `AgentMode::Public` builds a hardened agent with
/// only `web_search` and `web_fetch`.  See `docs/public-agents.md`.
///
/// Every controller should use this instead of building agents manually.
/// The mode is the single point of control — individual controllers just
/// declare the trust level, and this function handles the rest.
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    mode: AgentMode,
) -> crate::Result<crate::agent::Agent> {
    if mode == AgentMode::Public {
        return build_public_agent(settings, controller_prompt);
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

    let workspace: std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    let nudge_interval = {
        let ws = workspace.read().await;
        ws.nudge_interval()
    };

    let client = crate::llm::create_client(
        &agent_settings,
        Some(std::sync::Arc::clone(&workspace)),
        settings.dangerous_no_sandbox,
    );
    let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, settings.dangerous_no_sandbox);
    let skills = {
        let ws = workspace.read().await;
        crate::skill::create_skills(
            settings,
            Some(&**ws),
            std::sync::Arc::clone(&sandbox),
            Some(std::sync::Arc::clone(&workspace)),
        )
        .await
    };

    crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .build()
}

/// Build a public agent — restricted to web_search + web_fetch, no workspace
/// write access, sandbox always enforced.
///
/// The agent gets SOUL.md and IDENTITY.md injected into its system prompt
/// (read-only, in-memory only).  It does NOT receive a workspace reference,
/// so it cannot modify identity or memory files via workspace tools.
///
/// Called by `build_agent()` when `public == true`.  Kept as a separate
/// function for clarity, not because callers should use it directly.
fn build_public_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
) -> crate::Result<crate::agent::Agent> {
    let skills: Vec<Box<dyn crate::skill::Skill>> = vec![Box::new(
        crate::skill::builtin::BuiltinSkill::new_filtered(
            settings.web_search.as_ref(),
            &["web_search".into(), "web_fetch".into()],
        ),
    )];

    let mut agent_settings = settings.agent.clone();

    // Load the workspace read-only to extract SOUL.md and IDENTITY.md.
    // The workspace is dropped after reading — the public agent never gets
    // a workspace reference, so these files are read-only in-memory only.
    if let Ok(workspace) = crate::workspace::create_workspace(&settings.workspace) {
        let mut identity_parts: Vec<String> = Vec::new();

        for (label, file) in [("PERSONALITY", "SOUL.md"), ("IDENTITY", "IDENTITY.md")] {
            if let Some(content) = workspace.get(file) {
                if !content.trim().is_empty() {
                    identity_parts.push(format!("## {label}\n\n{content}"));
                }
            }
        }

        if !identity_parts.is_empty() {
            agent_settings.system_prompt.push_str("\n\n");
            agent_settings
                .system_prompt
                .push_str(&identity_parts.join("\n\n---\n\n"));
        }
        // workspace is dropped here — public agent has no write access.
    }

    agent_settings.system_prompt.push_str(
        "\n\nYou are a public-facing agent with limited tools. You can search \
         the web and fetch web pages to answer questions. You do NOT have access \
         to the filesystem, shell commands, or any workspace tools. Be concise \
         and cite your sources.",
    );

    if let Some(prompt) = controller_prompt {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(prompt);
    }

    let client = crate::llm::create_client(&agent_settings, None, false);
    // SECURITY: Always false — public agent sandbox is never disabled.
    let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, false);

    crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .build()
}

// ---------------------------------------------------------------------------
// Provider switching helpers
// ---------------------------------------------------------------------------

/// Build a new agent using a named provider, preserving conversation history.
///
/// Looks up `provider_name` in `settings.providers`, builds a new agent
/// with that provider's config, and restores the given messages.  When
/// `model` is `Some`, validates it against the provider's model list;
/// otherwise uses the provider's default (first) model.
pub async fn build_agent_with_provider(
    settings: &Settings,
    provider_name: &str,
    model: Option<&str>,
    controller_prompt: Option<&str>,
    existing_messages: Vec<crate::message::Message>,
) -> crate::Result<crate::agent::Agent> {
    let pc = settings.providers.get(provider_name).ok_or_else(|| {
        crate::error::DysonError::Config(format!("unknown provider '{provider_name}'"))
    })?;

    let resolved_model = match model {
        Some(m) => {
            if !pc.models.iter().any(|existing| existing == m) {
                let available = pc.models.join(", ");
                return Err(crate::error::DysonError::Config(format!(
                    "unknown model '{m}' for provider '{provider_name}'. Available: {available}"
                )));
            }
            m.to_string()
        }
        None => pc.default_model().to_string(),
    };

    // Build a modified settings with the new provider's fields.
    let mut switched = settings.clone();
    switched.agent.provider = pc.provider_type.clone();
    switched.agent.model = resolved_model;
    switched.agent.api_key = pc.api_key.clone();
    switched.agent.base_url = pc.base_url.clone();

    // Provider switching is only for private agents.
    let mut agent = build_agent(&switched, controller_prompt, AgentMode::Private).await?;
    agent.set_messages(existing_messages);
    Ok(agent)
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
        return Ok((first.to_string(), second.map(|s| s.to_string())));
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
    let workspace_path = crate::workspace::OpenClawWorkspace::resolve_path(Some(
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
pub async fn check_and_reload_agent(
    reloader: &mut crate::config::hot_reload::HotReloader,
    current_settings: &mut Settings,
    original_dangerous_no_sandbox: bool,
    agent: &mut crate::agent::Agent,
    current_provider: &mut String,
    current_model: &mut String,
    controller_prompt: Option<&str>,
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

    let messages = agent.messages().to_vec();
    match build_agent_with_provider(
        current_settings,
        current_provider,
        Some(current_model),
        controller_prompt,
        messages,
    )
    .await
    {
        Ok(a) => {
            *agent = a;
        }
        Err(_) => {
            // Provider/model removed from config — fall back to defaults.
            match build_agent(current_settings, controller_prompt, AgentMode::Private).await {
                Ok(a) => {
                    *agent = a;
                    *current_provider =
                        active_provider_name(current_settings).unwrap_or_default();
                    *current_model = current_settings.agent.model.clone();
                }
                Err(e) => return ReloadOutcome::Error(format!("reload error: {e}")),
            }
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
    /// Input was not a shared command — controller should handle it.
    NotHandled,
}

/// Execute a shared command, returning a result for the controller to render.
///
/// Handles: `/clear`, `/compact`, `/models`, `/model`.
/// Returns `NotHandled` for everything else, so controllers can check
/// their own commands before or after calling this.
#[allow(clippy::too_many_arguments)]
pub async fn execute_command(
    input: &str,
    agent: &mut crate::agent::Agent,
    output: &mut dyn Output,
    settings: &Settings,
    current_provider: &mut String,
    current_model: &mut String,
    config_path: Option<&Path>,
    controller_prompt: Option<&str>,
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

    if input == "/models" {
        if settings.providers.is_empty() {
            return CommandResult::ModelList {
                providers: Vec::new(),
            };
        }
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
                        active: name == current_provider.as_str() && m == current_model,
                    })
                    .collect(),
            })
            .collect();
        return CommandResult::ModelList { providers };
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
            current_provider,
        ) {
            Ok(parsed) => parsed,
            Err(e) => return CommandResult::ModelParseError(e),
        };
        let messages = agent.messages().to_vec();
        match build_agent_with_provider(
            settings,
            &target_provider,
            target_model.as_deref(),
            controller_prompt,
            messages,
        )
        .await
        {
            Ok(new_agent) => {
                *agent = new_agent;
                let pc = &settings.providers[&target_provider];
                let resolved = target_model
                    .as_deref()
                    .unwrap_or_else(|| pc.default_model())
                    .to_string();
                *current_model = resolved.clone();
                *current_provider = target_provider.clone();
                if let Some(cp) = config_path {
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

    CommandResult::NotHandled
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
            let name = entry.file_name().to_string_lossy().to_string();
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
    use std::io::Write;

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
        let mut content = String::new();
        for i in 0..500 {
            content.push_str(&format!("log line number {i:04}\n"));
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
}
