// ===========================================================================
// Dyson CLI — the binary entry point.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Parses command-line arguments, loads configuration, builds controllers
//   from the [[controller]] config, and runs them.  Controllers own the
//   full lifecycle: sourcing input, running the agent, delivering output.
//
// Why is main.rs thin?
//   All the real work lives in the library crate (lib.rs).  main.rs is
//   just wiring: parse args → load config → build controllers → run.
//
// Controller model:
//   Dyson supports multiple concurrent controllers.  Each [[controller]]
//   entry in dyson.toml becomes a Controller impl that runs as a tokio
//   task.  For example, you can run a terminal REPL and a Telegram bot
//   simultaneously.
//
//   If no [[controller]] entries exist, Dyson defaults to a single
//   terminal controller.
//
// Single-shot mode:
//   If a prompt is provided on the command line, Dyson bypasses the
//   controller system entirely: it creates a one-off agent, runs it,
//   prints the result, and exits.
//
// The --dangerous-no-sandbox flag:
//   Required in Phase 1 because no real sandbox exists yet.  Forces the
//   user to explicitly acknowledge that tool calls are unrestricted.
// ===========================================================================

use std::path::PathBuf;

use clap::Parser;

use dyson::config::{loader, LlmProvider};
use dyson::controller::Controller;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

/// Dyson — streaming AI agent framework.
#[derive(Parser)]
#[command(name = "dyson", about = "Streaming AI agent with tool use")]
struct Cli {
    /// Path to a dyson.toml config file.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run without any sandbox restrictions.
    #[arg(long)]
    dangerous_no_sandbox: bool,

    /// LLM provider override: "anthropic", "openai", or "claude-code".
    #[arg(long)]
    provider: Option<String>,

    /// Base URL override for the LLM API.
    #[arg(long)]
    base_url: Option<String>,

    /// Single-shot prompt.  If provided, bypasses controllers entirely.
    prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // -- Initialize tracing --
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // -- Require sandbox acknowledgment --
    if !cli.dangerous_no_sandbox {
        anyhow::bail!(
            "Dyson requires a sandbox mode.  In Phase 1, only --dangerous-no-sandbox is \
             available.\n\n\
             Usage: dyson --dangerous-no-sandbox [prompt]"
        );
    }

    // -- Load config --
    let mut settings = loader::load_settings(cli.config.as_deref())?;

    // CLI flags override config.
    if let Some(provider_str) = &cli.provider {
        settings.agent.provider = match provider_str.to_lowercase().as_str() {
            "anthropic" => LlmProvider::Anthropic,
            "openai" | "gpt" | "codex" => LlmProvider::OpenAi,
            "claude-code" | "claude_code" | "cc" => LlmProvider::ClaudeCode,
            other => anyhow::bail!(
                "unknown provider '{other}'.  Use 'anthropic', 'openai', or 'claude-code'."
            ),
        };
    }
    if let Some(base_url) = cli.base_url {
        settings.agent.base_url = Some(base_url);
    }

    tracing::info!(
        model = settings.agent.model,
        provider = ?settings.agent.provider,
        controllers = settings.controllers.len(),
        "configuration loaded"
    );

    // -- Single-shot mode --
    //
    // If a prompt was provided on the command line, skip the controller
    // system entirely: create a one-off agent, run it, print, exit.
    if let Some(prompt) = cli.prompt {
        let client = dyson::llm::create_client(&settings.agent);
        let sandbox: Box<dyn dyson::sandbox::Sandbox> =
            Box::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
        let skills = dyson::skill::create_skills(&settings).await;
        let mut agent = dyson::agent::Agent::new(client, sandbox, skills, &settings.agent)?;
        let mut output = dyson::controller::terminal::TerminalOutput::new();
        agent.run(&prompt, &mut output).await?;
        println!();
        return Ok(());
    }

    // -- Build controllers from config --
    //
    // Each [[controller]] entry becomes a Controller instance.
    // If no controllers are configured, default to terminal.
    let mut controllers: Vec<Box<dyn Controller>> = Vec::new();

    if settings.controllers.is_empty() {
        // No [[controller]] entries — default to terminal.
        controllers.push(Box::new(
            dyson::controller::terminal::TerminalController,
        ));
    } else {
        for config in &settings.controllers {
            match config.controller_type.as_str() {
                "terminal" => {
                    controllers.push(Box::new(
                        dyson::controller::terminal::TerminalController,
                    ));
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
                other => {
                    tracing::warn!(
                        controller_type = other,
                        "unknown controller type — skipping"
                    );
                }
            }
        }
    }

    // -- Run controllers --
    //
    // If there's exactly one controller, run it directly (simpler stack
    // traces, no spawning overhead).  If multiple, run them as concurrent
    // tokio tasks.
    if controllers.len() == 1 {
        let controller = controllers.into_iter().next().unwrap();
        tracing::info!(controller = controller.name(), "starting controller");
        controller.run(&settings).await?;
    } else {
        // Multiple controllers — run concurrently.
        let mut handles = Vec::new();

        for controller in controllers {
            let settings = settings.clone();
            let name = controller.name().to_string();

            tracing::info!(controller = name, "starting controller");

            handles.push(tokio::spawn(async move {
                if let Err(e) = controller.run(&settings).await {
                    tracing::error!(controller = name, error = %e, "controller failed");
                }
            }));
        }

        // Wait for all controllers to finish (they run until shutdown).
        for handle in handles {
            let _ = handle.await;
        }
    }

    Ok(())
}
