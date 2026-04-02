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
//   mod.rs      — Controller trait, Output trait (this file)
//   terminal.rs — Interactive terminal REPL
//   telegram.rs — Telegram bot
//
// Why "Controller" instead of "UI"?
//   "UI" implies visual rendering.  But a controller does more than render:
//   - It sources input (stdin, Telegram messages, HTTP requests, cron)
//   - It manages the agent lifecycle (create, run, conversation state)
//   - It delivers output (terminal, Telegram edits, webhooks)
//   - It enforces access control (Telegram allowed_chat_ids)
//
//   A Telegram bot isn't a "UI" — it's a controller that bridges Telegram's
//   message protocol to Dyson's agent loop.  A future Slack controller,
//   Discord controller, or HTTP API controller would do the same.
//
// How controllers fit in the architecture:
//
//   dyson.json "controllers" array
//     │
//     ▼
//   main.rs reads config, creates Controller instances
//     │
//     ├── TerminalController::run()   ← interactive REPL
//     ├── TelegramController::run()   ← Telegram bot polling
//     └── (future) HttpController     ← REST API server
//           │
//           ▼
//         Each controller creates its own Agent and Output
//         per session/message/request
//
// Multiple controllers:
//   Dyson supports running multiple controllers simultaneously.  For
//   example, you could run both a terminal REPL and a Telegram bot:
//
//   ```json
//   {
//     "controllers": [
//       { "type": "terminal" },
//       {
//         "type": "telegram",
//         "bot_token": "$TELEGRAM_API_KEY",
//         "allowed_chat_ids": [123456789]
//       }
//     ]
//   }
//   ```
//
//   Each controller runs as a concurrent tokio task.  They share the
//   same agent settings but maintain independent conversation state.
//
// The Output trait:
//   Output is the rendering half of the controller.  It's separated out
//   because the agent loop needs a render target (`&mut dyn Output`) but
//   doesn't care about input sourcing or lifecycle management.  The
//   controller creates an Output instance and passes it to the agent.
// ===========================================================================

pub mod telegram;
pub mod terminal;

use std::path::Path;

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
/// (terminal, Telegram, HTTP, etc.).
///
/// ## Lifecycle
///
/// ```text
/// main.rs creates controllers from config
///   → controller.run(settings).await
///     → (blocks until the controller shuts down)
///     → terminal: REPL loop reading stdin
///     → telegram: bot polling loop
///     → http: axum server (future)
/// ```
///
/// ## Concurrency
///
/// Multiple controllers run as concurrent tokio tasks.  Each is independent:
/// separate agent instances, separate conversation state, separate I/O.
#[async_trait::async_trait]
pub trait Controller: Send {
    /// Human-readable name for logging (e.g., "terminal", "telegram").
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
    /// controller-specific constraints.  For example, the Telegram
    /// controller tells the LLM not to use markdown because Telegram's
    /// MarkdownV2 parsing is fragile.
    fn system_prompt(&self) -> Option<&str> {
        None
    }
}

// ---------------------------------------------------------------------------
// Agent builder — shared logic for all controllers.
// ---------------------------------------------------------------------------

/// Build an agent from settings, loading the workspace into the system
/// prompt and applying any controller-specific prompt fragments.
///
/// Every controller should use this instead of building agents manually.
/// This ensures the workspace (SOUL.md, MEMORY.md, etc.) is always loaded
/// and that the workspace is available to tools via `ToolContext.workspace`.
///
/// The workspace backend is determined by `settings.workspace.backend`
/// (default: "openclaw") and loaded from `settings.workspace.connection_string`
/// (default: "~/.dyson/").  The workspace is wrapped in `Arc<RwLock>` so
/// tools can read and write workspace files concurrently.
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
) -> crate::Result<crate::agent::Agent> {
    // Load the persistent workspace via the configured backend.
    let workspace = crate::workspace::create_workspace(&settings.workspace)?;

    // Compose the system prompt: base + workspace + controller.
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

    // Wrap workspace in Arc<RwLock> for shared tool access.
    let workspace: std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    // Read nudge interval from workspace config.
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

    crate::agent::Agent::new(
        client,
        sandbox,
        skills,
        &agent_settings,
        Some(workspace),
        nudge_interval,
    )
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

    let mut agent = build_agent(&switched, controller_prompt).await?;
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
pub fn parse_model_command(
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

/// Format the provider list for display, marking the active model with `*`.
pub fn format_provider_list(
    settings: &Settings,
    current_provider: &str,
    current_model: &str,
) -> String {
    let mut providers: Vec<_> = settings.providers.iter().collect();
    providers.sort_by_key(|(name, _)| name.as_str());

    let mut out = String::from("Available providers:\n");
    for (name, pc) in &providers {
        out.push_str(&format!("  {} — {:?}\n", name, pc.provider_type));
        for model in &pc.models {
            let marker = if *name == current_provider && model == current_model {
                " *"
            } else {
                ""
            };
            out.push_str(&format!("    {model}{marker}\n"));
        }
    }
    out
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
// Output trait
// ---------------------------------------------------------------------------

/// Rendering interface for agent events.
///
/// The agent loop calls these methods as events occur.  Each controller
/// creates an appropriate Output implementation:
/// - `TerminalController` → `TerminalOutput` (writes to stdout)
/// - `TelegramController` → `TelegramOutput` (edits Telegram messages)
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
    /// Controllers implement this differently:
    /// - Terminal: prints the file path
    /// - Telegram: sends the file as a document via `sendDocument`
    fn send_file(&mut self, path: &Path) -> std::result::Result<(), DysonError>;

    /// An error occurred.
    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError>;

    /// Flush any buffered output.
    fn flush(&mut self) -> std::result::Result<(), DysonError>;
}
