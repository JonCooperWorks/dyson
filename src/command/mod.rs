// ===========================================================================
// Command — CLI subcommand implementations.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Each file in this module implements one CLI subcommand:
//
//   init.rs    — `dyson init`   — create ~/.dyson with default config
//   listen.rs  — `dyson listen` — start all configured controllers
//   run.rs     — `dyson run`    — single-shot: run once, print, exit
//
// Why split from main.rs?
//   main.rs defines the CLI structure (clap) and dispatches to these
//   functions.  Each command has enough logic to warrant its own file:
//   init handles directory scaffolding, systemd, OpenClaw import;
//   listen handles controller creation and concurrent execution;
//   run handles single-shot agent invocation.
//
//   This keeps main.rs short — just CLI parsing and dispatch.
// ===========================================================================

pub mod init;
pub mod listen;
pub mod run;

use std::path::PathBuf;

use dyson::config::Settings;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Default config path: ~/.dyson/dyson.json
///
/// Panics if the `HOME` environment variable is not set, since operating
/// without a home directory would produce confusing relative-path behaviour.
pub fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .expect("HOME environment variable must be set");
    PathBuf::from(home).join(".dyson").join("dyson.json")
}

/// Apply CLI overrides to loaded settings.
///
/// `--provider` selects a named provider from `settings.providers`.
/// If the name isn't found, it's treated as a provider type string
/// (e.g. "anthropic") for convenience.
pub fn apply_overrides(
    settings: &mut Settings,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> dyson::error::Result<()> {
    if let Some(provider_str) = provider {
        if let Some(pc) = settings.providers.get(&provider_str) {
            // Named provider from config.
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = pc.default_model().to_string();
            settings.agent.api_key = pc.api_key.clone();
            settings.agent.base_url = pc.base_url.clone();
        } else if let Some(provider_type) = dyson::llm::registry::from_str_loose(&provider_str) {
            // Bare provider type string (e.g. "anthropic").
            settings.agent.provider = provider_type;
        } else {
            let available: Vec<&str> = settings.providers.keys().map(|s| s.as_str()).collect();
            return Err(dyson::error::DysonError::Config(format!(
                "unknown provider '{provider_str}'.  \
                 Available: {available:?}.  \
                 Or use a type: '{}'.",
                dyson::llm::registry::all_canonical_names().join("', '"),
            )));
        }
    }
    if let Some(url) = base_url {
        settings.agent.base_url = Some(url);
    }
    settings.dangerous_no_sandbox = dangerous_no_sandbox;
    if let Some(ws) = workspace {
        settings.workspace.connection_string = dyson::auth::Credential::new(ws);
    }
    Ok(())
}
