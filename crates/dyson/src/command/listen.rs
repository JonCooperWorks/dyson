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
                "swarm" => {
                    #[cfg(feature = "dangerous_swarm")]
                    {
                        if let Some(mut ctrl) =
                            dyson::controller::swarm::SwarmController::from_config(config)
                        {
                            // Auto-inject the hub as an MCP skill so ALL agents
                            // (terminal, telegram, etc.) get swarm tools.
                            let swarm_config: dyson::config::SwarmControllerConfig =
                                match serde_json::from_value(config.config.clone()) {
                                    Ok(c) => c,
                                    Err(_) => {
                                        tracing::warn!("swarm controller config already parsed — skipping MCP auto-wire");
                                        controllers.push(Box::new(ctrl));
                                        continue;
                                    }
                                };

                            let node_name = swarm_config.node_name_or_default();
                            let hub_base = swarm_config.url.trim_end_matches('/');

                            // Wire a deferred bearer auth: the controller will
                            // publish the registration token after connecting to
                            // the hub, and the MCP skill will read it on each
                            // request for cryptographic caller verification.
                            let (token_tx, token_rx) = tokio::sync::watch::channel(None);
                            ctrl.set_token_channel(token_tx);
                            let deferred_auth: std::sync::Arc<dyn dyson::auth::Auth> =
                                std::sync::Arc::new(dyson::auth::DeferredBearerAuth::new(token_rx));

                            settings.skills.push(dyson::config::SkillConfig::Mcp(
                                Box::new(dyson::config::McpConfig {
                                    name: format!("swarm_{node_name}"),
                                    transport: dyson::config::McpTransportConfig::Http {
                                        // Caller identity is resolved from the
                                        // bearer token set via `DeferredBearerAuth`
                                        // below — no query params needed.
                                        url: format!("{hub_base}/mcp"),
                                        headers: std::collections::HashMap::new(),
                                        auth: None,
                                    },
                                    exclude_tools: vec![],
                                    custom_auth: Some(deferred_auth),
                                }),
                            ));

                            controllers.push(Box::new(ctrl));
                        } else {
                            tracing::warn!("swarm controller config invalid — skipping");
                        }
                    }
                    #[cfg(not(feature = "dangerous_swarm"))]
                    {
                        let _ = config;
                        tracing::warn!(
                            "swarm controller found in config, but this binary was \
                             not compiled with the `dangerous_swarm` feature — the \
                             swarm controller will not run and the swarm MCP tools \
                             will not be available.  Rebuild with \
                             `cargo build --features dangerous_swarm` to enable."
                        );
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
