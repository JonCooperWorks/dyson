//! Runtime settings publication, config paths, and agent reloads.
use super::{AgentMode, ClientRegistry, build_agent};
use crate::config::Settings;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Hot-reload setup — shared across all controllers.
// ---------------------------------------------------------------------------

/// Process-wide store for the resolved config-file path.  Set once by
/// `command::listen` (or `command::swarm` via `listen::run`) at
/// startup, read by every site that needs to know which `dyson.json`
/// the program is actually backed by — `create_hot_reloader`, the
/// HTTP controller's `HttpState::config_path` field, etc.
///
/// Why this exists: the original layer of code re-derived the path
/// from `std::env::args()` at every callsite.  In `dyson swarm` mode
/// the path is constructed internally (no `--config` flag, systemd cwd
/// is `/`), so each callsite's argv parse returned `None` and silent
/// path-resolution-fallout shipped twice — once breaking program-level
/// hot-reload, once breaking `state.config_path()` for
/// `/api/admin/configure`'s `patch_models_in_config` step.  Funnelling
/// through a single OnceLock kills the failure mode at the source.
pub(crate) static EXPLICIT_CONFIG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install the program's resolved config path.  First writer wins
/// (`OnceLock` semantics) so a stray double-install in tests is a
/// no-op rather than a panic.
pub fn install_explicit_config_path(p: PathBuf) {
    let _ = EXPLICIT_CONFIG_PATH.set(p);
}

/// Resolve the config-file path for any caller that needs it.
/// Order: caller-supplied `explicit` → installed OnceLock →
/// `--config` / `-c` argv flag → `./dyson.json`.  Returns `None` when
/// no source has a path (legitimate for in-memory dev / tests).
pub fn resolve_config_path_for_runtime(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
    explicit
        .map(PathBuf::from)
        .or_else(|| EXPLICIT_CONFIG_PATH.get().cloned())
        .or_else(|| {
            std::env::args()
                .skip_while(|a| a != "--config" && a != "-c")
                .nth(1)
                .map(PathBuf::from)
        })
        .or_else(|| {
            let p = PathBuf::from("dyson.json");
            if p.exists() { Some(p) } else { None }
        })
}

/// Resolve the config file path and create a hot reloader.
///
/// `explicit` is the config path the caller already resolved.  When
/// set, it wins outright; otherwise we fall through to the
/// process-wide `EXPLICIT_CONFIG_PATH`, then argv, then a `dyson.json`
/// in cwd.  See `EXPLICIT_CONFIG_PATH`'s docstring for the full bug
/// history this layered resolution is defending against.
pub fn create_hot_reloader(
    settings: &Settings,
    explicit: Option<&std::path::Path>,
) -> (Option<PathBuf>, crate::config::hot_reload::HotReloader) {
    let config_path = resolve_config_path_for_runtime(explicit);
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
// Program-level settings broadcast — single hot-reload source for all
// controllers.
// ---------------------------------------------------------------------------

/// Global publisher handle for the program-wide settings watcher.
/// `command::listen` installs a `tokio::sync::watch::Sender` here
/// before starting controllers; each controller that wants to react
/// to hot-reloads calls `subscribe_settings_updates()` during its
/// `run()` and gets a matching `Receiver`.  Stored via a `OnceLock`
/// so controllers don't need to thread the handle through method
/// signatures — the trait stays stable for the 4+ impls.
pub(crate) static SETTINGS_BUS: std::sync::OnceLock<
    tokio::sync::watch::Sender<std::sync::Arc<Settings>>,
> = std::sync::OnceLock::new();

/// Install the program-level settings publisher.  Called once by
/// `command::listen` before controllers spawn.  Subsequent calls are
/// ignored (the `OnceLock` keeps the first writer).
pub fn install_settings_bus(initial: std::sync::Arc<Settings>) {
    let (tx, _rx) = tokio::sync::watch::channel(initial);
    let _ = SETTINGS_BUS.set(tx);
}

/// Publish a fresh settings snapshot to everyone subscribed.  No-op
/// when the bus hasn't been installed (most tests).
pub fn publish_settings(new: std::sync::Arc<Settings>) {
    if let Some(tx) = SETTINGS_BUS.get() {
        let _ = tx.send(new);
    }
}

/// Subscribe to settings updates.  `None` when the bus hasn't been
/// installed — e.g. an http_controller integration test that stands
/// up `HttpState` directly without going through `command::listen`.
pub fn subscribe_settings_updates() -> Option<tokio::sync::watch::Receiver<std::sync::Arc<Settings>>>
{
    SETTINGS_BUS
        .get()
        .map(tokio::sync::watch::Sender::subscribe)
}

// ---------------------------------------------------------------------------
// Cross-controller artefact publishing — so a file sent by one controller
// (e.g. Telegram) shows up as an artefact in another controller's view
// (e.g. the HTTP web UI).
// ---------------------------------------------------------------------------

/// Publishes a file sent through one controller as a first-class artefact
/// in a browser-facing controller.  Implemented by `HttpState`; installed
/// on startup so the Telegram controller can call it when its `send_file`
/// fires and the same chat is visible in the web UI.
pub trait BrowserArtefactSink: Send + Sync {
    /// Stash `path` as an artefact for `chat_id`.  Best-effort: errors
    /// (file gone, too big, etc.) are logged by the implementation, not
    /// propagated — the caller has already delivered the file through
    /// its primary channel and just wants the browser copy as a bonus.
    fn publish_file_as_artefact(&self, chat_id: &str, path: &std::path::Path);
}

pub(crate) static BROWSER_ARTEFACT_SINK: std::sync::OnceLock<
    std::sync::Arc<dyn BrowserArtefactSink>,
> = std::sync::OnceLock::new();

/// Install the browser artefact sink.  Called by the HTTP controller on
/// startup.  Subsequent calls are ignored (first writer wins, matching
/// `SETTINGS_BUS`).
pub fn install_browser_artefact_sink(sink: std::sync::Arc<dyn BrowserArtefactSink>) {
    let _ = BROWSER_ARTEFACT_SINK.set(sink);
}

/// Look up the installed browser artefact sink.  `None` when no HTTP
/// controller is running — the caller should treat this as a no-op
/// (nothing to bridge to).
pub fn browser_artefact_sink() -> Option<std::sync::Arc<dyn BrowserArtefactSink>> {
    BROWSER_ARTEFACT_SINK.get().cloned()
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
    original_sandbox_bypass: Option<&crate::sandbox::SandboxBypassGuard>,
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
        current_settings.sandbox_bypass = original_sandbox_bypass.cloned();
    }

    // Reload the client registry so new API keys / base URLs take effect.
    registry.reload(current_settings, None);

    let messages = agent.messages().to_vec();
    let client = registry.get_default();
    match build_agent(
        current_settings,
        controller_prompt,
        AgentMode::Private,
        client,
        registry,
        None,
    )
    .await
    {
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
