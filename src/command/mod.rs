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

use dyson::config::{LlmProvider, Settings};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Default config path: ~/.dyson/dyson.json
pub fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".dyson").join("dyson.json")
}

/// Apply CLI overrides to loaded settings.
pub fn apply_overrides(
    settings: &mut Settings,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    if let Some(provider_str) = provider {
        settings.agent.provider = match provider_str.to_lowercase().as_str() {
            "anthropic" => LlmProvider::Anthropic,
            "openai" | "gpt" | "codex" => LlmProvider::OpenAi,
            "claude-code" | "claude_code" | "cc" => LlmProvider::ClaudeCode,
            other => anyhow::bail!(
                "unknown provider '{other}'.  Use 'anthropic', 'openai', or 'claude-code'."
            ),
        };
    }
    if let Some(url) = base_url {
        settings.agent.base_url = Some(url);
    }
    settings.dangerous_no_sandbox = dangerous_no_sandbox;
    if let Some(ws) = workspace {
        settings.workspace.connection_string = ws;
    }
    Ok(())
}
