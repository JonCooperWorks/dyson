// ===========================================================================
// Dyson CLI — the binary entry point.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides the `dyson` command with subcommands:
//
//   dyson listen          — start all configured controllers (Telegram, terminal, etc.)
//   dyson init --noinput  — create ~/.dyson with default config and workspace
//   dyson run "prompt"    — single-shot: run once, print, exit
//
// Directory layout (~/.dyson/):
//
//   ~/.dyson/
//     dyson.json           — main config file
//     workspace/
//       SOUL.md            — personality
//       IDENTITY.md        — who the agent is
//       MEMORY.md          — long-term memory
//       AGENTS.md          — operating procedures
//       HEARTBEAT.md       — periodic tasks
//       memory/
//         2026-03-19.md    — daily journal
//
// `dyson init` creates this structure.  `dyson listen` reads from it.
//
// Module layout:
//   main.rs          — CLI definition and dispatch (this file)
//   command/
//     mod.rs         — shared helpers (config path resolution, overrides)
//     init.rs        — `dyson init`
//     listen.rs      — `dyson listen`
//     run.rs         — `dyson run`
// ===========================================================================

mod command;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Dyson — streaming AI agent framework.
#[derive(Parser)]
#[command(name = "dyson", about = "Streaming AI agent with tool use")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start all configured controllers (Telegram, terminal, etc.).
    Listen {
        /// Path to dyson.json config file.
        /// Default: ~/.dyson/dyson.json
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Run without any sandbox restrictions.
        #[arg(long)]
        dangerous_no_sandbox: bool,

        /// LLM provider override.
        #[arg(long)]
        provider: Option<String>,

        /// Base URL override for the LLM API.
        #[arg(long)]
        base_url: Option<String>,

        /// Workspace directory override.
        #[arg(long)]
        workspace: Option<String>,
    },

    /// Initialize ~/.dyson with default config and workspace.
    Init {
        /// Skip interactive prompts — use all defaults.
        #[arg(long)]
        noinput: bool,

        /// Also install a systemd service (Linux only).
        /// Creates /etc/systemd/system/dyson.service, enables and starts it.
        /// Requires sudo (will elevate temporarily).
        #[arg(long)]
        daemonize: bool,

        /// Import an existing OpenClaw workspace directory.
        /// Copies its contents into ~/.dyson/workspace/.
        #[arg(long)]
        import_openclaw: Option<PathBuf>,

        /// Directory to initialize.  Default: ~/.dyson
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Run a single prompt and exit.
    Run {
        /// The prompt to run.
        prompt: String,

        /// Path to dyson.json config file.
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Run without any sandbox restrictions.
        #[arg(long)]
        dangerous_no_sandbox: bool,

        /// LLM provider override.
        #[arg(long)]
        provider: Option<String>,

        /// Base URL override for the LLM API.
        #[arg(long)]
        base_url: Option<String>,

        /// Workspace directory override.
        #[arg(long)]
        workspace: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { noinput, daemonize, import_openclaw, path } => {
            command::init::run(noinput, daemonize, import_openclaw, path)
        }
        Commands::Listen {
            config,
            dangerous_no_sandbox,
            provider,
            base_url,
            workspace,
        } => {
            command::listen::run(config, dangerous_no_sandbox, provider, base_url, workspace).await
        }
        Commands::Run {
            prompt,
            config,
            dangerous_no_sandbox,
            provider,
            base_url,
            workspace,
        } => {
            command::run::run(
                prompt,
                config,
                dangerous_no_sandbox,
                provider,
                base_url,
                workspace,
            )
            .await
        }
    }
}
