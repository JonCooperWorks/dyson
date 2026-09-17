//! Interaction channels and the contracts they share.
//!
//! `Controller` owns an input loop; `Output` renders agent events. The client
//! registry shares rate limits, the builder defines agent trust levels, runtime
//! manages reloads, and commands dispatches built-in operations. Channel modules
//! adapt those contracts to HTTP, Telegram, and terminal transports.

pub mod activity;
pub mod background;
pub mod http;
pub mod recording;
pub mod slash;
pub mod telegram;
pub mod terminal;

pub use activity::{
    ActivityEntry, ActivityHandle, ActivityRegistry, ActivityStatus, ActivityToken, LANE_SUBAGENT,
    truncate_note,
};

/// Sentinel model/key value a freshly-provisioned instance carries before
/// its first real `/llm` call swaps in operator-supplied credentials.  The
/// readiness checks treat its presence as "not warmed up yet"; the swarm
/// provisioning path and the slash-command warmup both stamp it.  One
/// definition so a typo can't silently break readiness on one side.
pub const WARMUP_PLACEHOLDER: &str = "warmup-placeholder";

use crate::config::Settings;

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

/// Find the provider name in `settings.providers` that is active for this
/// settings snapshot.
pub fn active_provider_name(settings: &Settings) -> Option<String> {
    if let Some(active) = settings.active_provider.as_ref()
        && settings.providers.contains_key(active.name())
    {
        return Some(active.name().to_string());
    }

    // Compatibility for Settings built by tests or older call sites that
    // populate only the flattened agent fields.
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

mod client_registry;
pub use client_registry::ClientRegistry;

mod agent_builder;
pub use agent_builder::build_agent;

mod runtime;
pub use runtime::{
    BrowserArtefactSink, ReloadOutcome, browser_artefact_sink, check_and_reload_agent,
    create_hot_reloader, install_browser_artefact_sink, install_explicit_config_path,
    install_settings_bus, publish_settings, resolve_config_path_for_runtime,
    subscribe_settings_updates,
};

mod commands;
pub use commands::{
    BackgroundCompletion, CommandResult, ModelInfo, ModelState, ProviderInfo,
    execute_agent_command, execute_lockfree_command,
};

mod background_run;
pub(crate) use background_run::spawn_background_agent;
pub use background_run::{format_background_result, persist_background_result};

mod log_tail;
pub use log_tail::read_log_tail;

mod output;
pub use output::Output;
pub(crate) use output::completed_text;

#[cfg(test)]
mod tests;
