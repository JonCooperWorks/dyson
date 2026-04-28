// ===========================================================================
// dyson listen — start all configured controllers.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Loads config, creates controller instances (Terminal, Telegram, etc.),
//   and runs them.  If there's only one controller, it runs directly.
//   If there are multiple, they run as concurrent tokio tasks.
//
// How it works:
//
//   1. Resolve config path: explicit --config > ~/.dyson/dyson.json > ./dyson.json
//   2. Load settings from config file
//   3. Apply CLI overrides (provider, base_url, workspace, no_sandbox)
//   4. Build controller instances from config entries
//   5. Run controllers (single = direct, multiple = concurrent tasks)
//
// Why controllers run concurrently:
//   You might want both a terminal REPL and a Telegram bot running
//   from the same config.  Each controller is independent — separate
//   agent instances, separate conversation state, separate I/O.
// ===========================================================================

use std::path::PathBuf;

use dyson::controller::Controller;

/// Run `dyson listen`.
pub async fn run(
    config: Option<PathBuf>,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> dyson::error::Result<()> {
    let config_path = super::resolve_config_path(config);

    let mut settings = dyson::config::loader::load_settings(config_path.as_deref())?;
    super::apply_overrides(
        &mut settings,
        dangerous_no_sandbox,
        provider,
        base_url,
        workspace,
    )?;

    tracing::info!(
        model = settings.agent.model,
        provider = ?settings.agent.provider,
        controllers = settings.controllers.len(),
        "configuration loaded"
    );

    // Single shared client registry — one LLM client per provider,
    // shared across all controllers and surviving provider switches.
    let registry = std::sync::Arc::new(dyson::controller::ClientRegistry::new(&settings, None));

    // Build controllers.
    let mut controllers: Vec<Box<dyn Controller>> = Vec::new();

    if settings.controllers.is_empty() {
        return Err(dyson::error::DysonError::Config(
            "no controllers configured.  Add a controller to the \"controllers\" array in dyson.json.\n\
             Use 'dyson run \"prompt\"' for single-shot mode.".into()
        ));
    } else {
        for config in &settings.controllers {
            match config.controller_type.as_str() {
                "terminal" => {
                    controllers.push(Box::new(dyson::controller::terminal::TerminalController));
                }
                "telegram" => {
                    if let Some(ctrl) =
                        dyson::controller::telegram::TelegramController::from_config(config)
                    {
                        controllers.push(Box::new(ctrl));
                    } else {
                        tracing::warn!("telegram controller missing bot_token — skipping");
                    }
                }
                "http" => {
                    if let Some(ctrl) =
                        dyson::controller::http::HttpController::from_config(config)
                    {
                        controllers.push(Box::new(ctrl));
                    } else {
                        tracing::warn!("http controller config invalid — skipping");
                    }
                }
                other => {
                    tracing::warn!(
                        controller_type = other,
                        "unknown controller type — skipping"
                    );
                }
            }
        }
    }

    // Run controllers, racing against shutdown signals (SIGINT / SIGTERM).
    if controllers.is_empty() {
        return Err(dyson::error::DysonError::Config(
            "no valid controllers could be created from the configuration".into(),
        ));
    }

    // Install the program-level hot-reload broadcast so controllers
    // can subscribe for live settings updates instead of each spinning
    // up its own file watcher.  The companion task below does the
    // actual polling + registry reload + publish.
    //
    // Also publish the resolved config path via the controller-level
    // OnceLock so callers like the HTTP controller (which can't get
    // it through its run() signature) see the same path the listen
    // command resolved.  See `controller::EXPLICIT_CONFIG_PATH`.
    dyson::controller::install_settings_bus(std::sync::Arc::new(settings.clone()));
    if let Some(p) = config_path.as_ref() {
        dyson::controller::install_explicit_config_path(p.clone());
    }
    spawn_program_hot_reload_task(
        &settings,
        config_path.as_deref(),
        std::sync::Arc::clone(&registry),
    );

    let shutdown = async {
        // Wait for Ctrl-C (SIGINT).
        let ctrl_c = tokio::signal::ctrl_c();

        // On Unix, also listen for SIGTERM for graceful container shutdown.
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            // INVARIANT: signal handler registration only fails if the OS
            // signal subsystem is broken — fatal, no recovery possible.
            .expect("failed to register SIGTERM handler");

            tokio::select! {
                _ = ctrl_c => tracing::info!("received SIGINT"),
                _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            }
        }

        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("received SIGINT");
        }
    };

    if controllers.len() == 1 {
        let controller = controllers.into_iter().next().expect("length checked above");
        tracing::info!(controller = controller.name(), "starting controller");
        tokio::select! {
            result = controller.run(&settings, &registry) => { result?; }
            _ = shutdown => {
                tracing::info!("shutting down");
            }
        }
    } else {
        let mut handles = Vec::new();
        for controller in controllers {
            let settings = settings.clone();
            let registry = std::sync::Arc::clone(&registry);
            let name = controller.name().to_string();
            tracing::info!(controller = name, "starting controller");
            handles.push(tokio::spawn(async move {
                if let Err(e) = controller.run(&settings, &registry).await {
                    tracing::error!(controller = name, error = %e, "controller failed");
                }
            }));
        }

        // Wait for either all controllers to finish or a shutdown signal.
        tokio::select! {
            _ = async {
                for handle in &mut handles {
                    let _ = handle.await;
                }
            } => {}
            _ = shutdown => {
                tracing::info!("shutting down — aborting controllers");
                for handle in &handles {
                    handle.abort();
                }
                // Give controllers a grace period to clean up.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }

    Ok(())
}

/// Spawn the program-level hot-reload task.  One per process; reacts
/// to `dyson.json` changes by:
///
/// 1. Reloading the shared `ClientRegistry` so new API keys / base
///    URLs take effect across every controller and every chat.
/// 2. Publishing the fresh `Settings` onto the `controller::SETTINGS_BUS`
///    watch channel so subscribed controllers (HTTP, Telegram) can
///    react: HTTP refreshes its in-memory snapshot, Telegram rebuilds
///    its agents.  Controllers that don't subscribe (terminal) keep
///    the initial snapshot — they run a single agent and a fresh
///    start picks up whatever's on disk anyway.
///
/// Skipped when no config path was resolved (in-memory dev scenarios).
fn spawn_program_hot_reload_task(
    settings: &dyson::config::Settings,
    explicit_config: Option<&std::path::Path>,
    registry: std::sync::Arc<dyson::controller::ClientRegistry>,
) {
    let (config_path, mut reloader) =
        dyson::controller::create_hot_reloader(settings, explicit_config);
    if config_path.is_none() {
        tracing::debug!("no config path — program-level hot-reload disabled");
        return;
    }
    let dangerous_no_sandbox = settings.dangerous_no_sandbox;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match reloader.check().await {
                Ok((true, Some(mut new_settings))) => {
                    // CLI-only flag, not in the JSON — preserve.
                    new_settings.dangerous_no_sandbox = dangerous_no_sandbox;
                    registry.reload(&new_settings, None);
                    dyson::controller::publish_settings(std::sync::Arc::new(new_settings));
                    tracing::info!("dyson.json hot-reloaded — subscribed controllers notified");
                }
                Ok((true, None)) => {
                    tracing::debug!("hot-reload detected change but settings unreadable");
                }
                Ok((false, _)) => {}
                Err(e) => tracing::debug!(error = %e, "hot-reload check failed"),
            }
        }
    });
}
