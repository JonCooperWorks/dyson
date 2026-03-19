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
//   dyson.toml [[controller]] entries
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
//   ```toml
//   [[controller]]
//   type = "terminal"
//
//   [[controller]]
//   type = "telegram"
//   bot_token = "$TELEGRAM_API_KEY"
//   allowed_chat_ids = [123456789]
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
/// This ensures the workspace (SOUL.md, MEMORY.md, etc.) is always loaded.
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
) -> crate::Result<crate::agent::Agent> {
    // Load the persistent workspace.
    let workspace = crate::persistence::Workspace::load_default(
        settings.workspace_path.as_deref(),
    )?;

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

    let client = crate::llm::create_client(&agent_settings);
    let sandbox = crate::sandbox::create_sandbox(
        &settings.sandbox,
        settings.dangerous_no_sandbox,
    );
    let skills = crate::skill::create_skills(settings).await;

    crate::agent::Agent::new(client, sandbox, skills, &agent_settings)
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

    /// An error occurred.
    fn error(&mut self, error: &DysonError) -> std::result::Result<(), DysonError>;

    /// Flush any buffered output.
    fn flush(&mut self) -> std::result::Result<(), DysonError>;
}
