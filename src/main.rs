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
// ===========================================================================

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use dyson::config::{loader, LlmProvider};
use dyson::controller::Controller;

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
        Commands::Init { noinput, daemonize, path } => cmd_init(noinput, daemonize, path),
        Commands::Listen {
            config,
            dangerous_no_sandbox,
            provider,
            base_url,
            workspace,
        } => {
            cmd_listen(config, dangerous_no_sandbox, provider, base_url, workspace).await
        }
        Commands::Run {
            prompt,
            config,
            dangerous_no_sandbox,
            provider,
            base_url,
            workspace,
        } => {
            cmd_run(
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

// ---------------------------------------------------------------------------
// dyson init
// ---------------------------------------------------------------------------

fn cmd_init(noinput: bool, daemonize: bool, path: Option<PathBuf>) -> anyhow::Result<()> {
    let home = std::env::var("HOME")?;
    let base = path.unwrap_or_else(|| PathBuf::from(&home).join(".dyson"));

    if !noinput {
        anyhow::bail!(
            "interactive init not yet implemented.  Use --noinput for defaults.\n\
             Usage: dyson init --noinput"
        );
    }

    eprintln!("initializing {}...", base.display());

    // Create directory structure.
    let workspace_dir = base.join("workspace");
    std::fs::create_dir_all(&workspace_dir)?;
    std::fs::create_dir_all(workspace_dir.join("memory"))?;

    // Write default config.
    let config_path = base.join("dyson.json");
    if !config_path.exists() {
        let default_config = serde_json::json!({
            "agent": {
                "provider": "claude-code",
                "model": "sonnet",
                "max_iterations": 20,
                "max_tokens": 8192
            },
            "workspace": {
                "path": workspace_dir.to_string_lossy()
            },
            "controllers": [
                { "type": "terminal" }
            ],
            "skills": {
                "builtin": {
                    "tools": ["bash"]
                }
            }
        });

        let json = serde_json::to_string_pretty(&default_config)?;
        std::fs::write(&config_path, format!("{json}\n"))?;
        eprintln!("  created {}", config_path.display());
    } else {
        eprintln!("  {} already exists — skipping", config_path.display());
    }

    // Create default workspace files.
    // Workspace::load() creates defaults if they don't exist.
    let _ = dyson::persistence::Workspace::load(&workspace_dir)?;
    eprintln!("  created workspace at {}", workspace_dir.display());

    // Install binary to PATH.
    install_to_path(&base)?;

    if daemonize {
        install_systemd_service(&base, &config_path)?;
    } else {
        eprintln!();
        eprintln!("done. to start:");
        eprintln!("  dyson listen --config {}", config_path.display());
        eprintln!();
        eprintln!("to install as a service:");
        eprintln!("  dyson init --noinput --daemonize");
    }

    Ok(())
}

/// Copy the dyson binary into ~/.dyson/bin/ and symlink to ~/.local/bin/
/// so it's on PATH without modifying shell configs.
///
/// ~/.local/bin/ is on PATH by default on most Linux distros (via
/// systemd's user environment) and on macOS if the user has it configured.
/// If ~/.local/bin/ doesn't exist, we create it — the user may need to
/// add it to PATH manually (we print instructions).
fn install_to_path(base: &PathBuf) -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;

    // Copy binary into ~/.dyson/bin/ (our own managed copy).
    let bin_dir = base.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let installed_bin = bin_dir.join("dyson");
    std::fs::copy(&current_exe, &installed_bin)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installed_bin, std::fs::Permissions::from_mode(0o755))?;
    }

    eprintln!("  installed binary to {}", installed_bin.display());

    // Symlink into ~/.local/bin/ so it's on PATH.
    let home = std::env::var("HOME").unwrap_or_default();
    let local_bin = PathBuf::from(&home).join(".local/bin");
    std::fs::create_dir_all(&local_bin)?;

    let symlink_path = local_bin.join("dyson");

    // Remove existing symlink/file if present.
    if symlink_path.exists() || symlink_path.is_symlink() {
        std::fs::remove_file(&symlink_path)?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&installed_bin, &symlink_path)?;

    eprintln!("  symlinked {} -> {}", symlink_path.display(), installed_bin.display());

    // Check if ~/.local/bin is actually on PATH.
    let path_var = std::env::var("PATH").unwrap_or_default();
    if !path_var.split(':').any(|p| PathBuf::from(p) == local_bin) {
        eprintln!();
        eprintln!("  note: {} is not on your PATH.", local_bin.display());
        eprintln!("  add this to your shell config (~/.bashrc or ~/.zshrc):");
        eprintln!("    export PATH=\"$HOME/.local/bin:$PATH\"");
    }

    Ok(())
}

/// Install a systemd user service for Dyson.
///
/// Creates ~/.config/systemd/user/dyson.service and enables it.
/// Falls back to /etc/systemd/system/dyson.service with sudo if
/// user services aren't available.
#[allow(unused_variables)]
fn install_systemd_service(base: &PathBuf, config_path: &PathBuf) -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("--daemonize is only supported on Linux (systemd).");
        eprintln!("on macOS, use launchd instead (not yet implemented).");
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        // Find the dyson binary.
        let dyson_bin = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("dyson"));

        let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
        let home = std::env::var("HOME").unwrap_or_default();

        // Build the service unit file.
        let unit = format!(
            "[Unit]\n\
             Description=Dyson AI Agent\n\
             After=network.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={dyson_bin} listen --config {config_path}\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             WorkingDirectory={home}\n\
             Environment=HOME={home}\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            dyson_bin = dyson_bin.display(),
            config_path = config_path.display(),
            home = home,
        );

        // Try user service first (no sudo needed).
        let user_service_dir = PathBuf::from(&home)
            .join(".config/systemd/user");
        let user_service_path = user_service_dir.join("dyson.service");

        eprintln!("installing systemd service...");

        if std::fs::create_dir_all(&user_service_dir).is_ok() {
            std::fs::write(&user_service_path, &unit)?;
            eprintln!("  created {}", user_service_path.display());

            // Enable and start.
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "enable", "dyson"])
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "start", "dyson"])
                .status();

            eprintln!("  enabled and started (user service)");
            eprintln!();
            eprintln!("manage with:");
            eprintln!("  systemctl --user status dyson");
            eprintln!("  systemctl --user restart dyson");
            eprintln!("  journalctl --user -u dyson -f");
        } else {
            // Fall back to system service with sudo.
            eprintln!("  user service dir not available, using system service (needs sudo)");

            let system_path = PathBuf::from("/etc/systemd/system/dyson.service");
            let unit_system = format!(
                "[Unit]\n\
                 Description=Dyson AI Agent\n\
                 After=network.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 User={user}\n\
                 ExecStart={dyson_bin} listen --config {config_path}\n\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 WorkingDirectory={home}\n\
                 Environment=HOME={home}\n\
                 \n\
                 [Install]\n\
                 WantedBy=multi-user.target\n",
                user = user,
                dyson_bin = dyson_bin.display(),
                config_path = config_path.display(),
                home = home,
            );

            // Write via sudo tee.
            let mut child = std::process::Command::new("sudo")
                .args(["tee", &system_path.to_string_lossy()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()?;

            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin.write_all(unit_system.as_bytes())?;
            }
            child.wait()?;

            eprintln!("  created {}", system_path.display());

            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "daemon-reload"])
                .status();
            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "enable", "dyson"])
                .status();
            let _ = std::process::Command::new("sudo")
                .args(["systemctl", "start", "dyson"])
                .status();

            eprintln!("  enabled and started (system service)");
            eprintln!();
            eprintln!("manage with:");
            eprintln!("  sudo systemctl status dyson");
            eprintln!("  sudo systemctl restart dyson");
            eprintln!("  sudo journalctl -u dyson -f");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// dyson listen
// ---------------------------------------------------------------------------

async fn cmd_listen(
    config: Option<PathBuf>,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    // Resolve config path: explicit > ~/.dyson/dyson.json > ./dyson.json
    let config_path = config.or_else(|| {
        let home_config = dirs_config_path();
        if home_config.exists() {
            Some(home_config)
        } else {
            let cwd = PathBuf::from("dyson.json");
            if cwd.exists() { Some(cwd) } else { None }
        }
    });

    let mut settings = loader::load_settings(config_path.as_deref())?;
    apply_overrides(&mut settings, dangerous_no_sandbox, provider, base_url, workspace)?;

    tracing::info!(
        model = settings.agent.model,
        provider = ?settings.agent.provider,
        controllers = settings.controllers.len(),
        "configuration loaded"
    );

    // Build controllers.
    let mut controllers: Vec<Box<dyn Controller>> = Vec::new();

    if settings.controllers.is_empty() {
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

    // Run controllers.
    if controllers.len() == 1 {
        let controller = controllers.into_iter().next().unwrap();
        tracing::info!(controller = controller.name(), "starting controller");
        controller.run(&settings).await?;
    } else {
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
        for handle in handles {
            let _ = handle.await;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// dyson run
// ---------------------------------------------------------------------------

async fn cmd_run(
    prompt: String,
    config: Option<PathBuf>,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> anyhow::Result<()> {
    let config_path = config.or_else(|| {
        let home_config = dirs_config_path();
        if home_config.exists() {
            Some(home_config)
        } else {
            let cwd = PathBuf::from("dyson.json");
            if cwd.exists() { Some(cwd) } else { None }
        }
    });

    let mut settings = loader::load_settings(config_path.as_deref())?;
    apply_overrides(&mut settings, dangerous_no_sandbox, provider, base_url, workspace)?;

    let workspace = dyson::persistence::Workspace::load_default(
        settings.workspace_path.as_deref(),
    )?;
    let mut agent_settings = settings.agent.clone();
    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    let client = dyson::llm::create_client(&agent_settings);
    let sandbox = dyson::sandbox::create_sandbox(
        &settings.sandbox,
        settings.dangerous_no_sandbox,
    );
    let skills = dyson::skill::create_skills(&settings).await;
    let mut agent = dyson::agent::Agent::new(client, sandbox, skills, &agent_settings)?;
    let mut output = dyson::controller::terminal::TerminalOutput::new();
    agent.run(&prompt, &mut output).await?;
    println!();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default config path: ~/.dyson/dyson.json
fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".dyson").join("dyson.json")
}

/// Apply CLI overrides to loaded settings.
fn apply_overrides(
    settings: &mut dyson::config::Settings,
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
        settings.workspace_path = Some(ws);
    }
    Ok(())
}
