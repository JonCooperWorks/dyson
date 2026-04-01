// ===========================================================================
// Terminal controller — interactive REPL on stdin/stdout.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `Controller` trait for interactive terminal sessions.
//   Reads user input from stdin, runs the agent, and streams output to
//   stdout.  This is the default controller when no others are configured.
//
// How it works:
//
//   TerminalController::run()
//     │
//     ├── create Agent from settings
//     ├── loop:
//     │     ├── print "> " prompt
//     │     ├── read line from stdin
//     │     ├── agent.run(input, &mut TerminalOutput)
//     │     │     ├── output.text_delta("Hello") → write to stdout
//     │     │     ├── output.tool_use_start(...)  → "[Tool: bash]"
//     │     │     └── output.flush()
//     │     └── repeat
//     └── exit on /exit, /quit, or Ctrl-D
//
// Why a Controller and not just code in main.rs?
//   By extracting the REPL into a Controller, it can coexist with other
//   controllers.  You could run both a terminal REPL and a Telegram bot
//   simultaneously — each as a concurrent task.
// ===========================================================================

use std::io::Write;
use std::path::Path;

use crate::config::Settings;
use crate::controller::Output;
use crate::error::DysonError;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// TerminalController
// ---------------------------------------------------------------------------

/// Interactive terminal REPL controller.
///
/// Reads from stdin, writes to stdout, runs until the user types `/exit`
/// or sends EOF (Ctrl-D).
pub struct TerminalController;

#[async_trait::async_trait]
impl super::Controller for TerminalController {
    fn name(&self) -> &str {
        "terminal"
    }

    async fn run(&self, settings: &Settings) -> crate::Result<()> {
        let mut current_settings = settings.clone();
        let mut agent = super::build_agent(&current_settings, None).await?;
        let mut output = TerminalOutput::new();

        // Track the active provider and model for within-provider switching.
        let mut current_provider = super::active_provider_name(&current_settings)
            .unwrap_or_default();
        let mut current_model = current_settings.agent.model.clone();

        // Hot reload: watch config file + workspace files.
        let config_path = std::env::args()
            .skip_while(|a| a != "--config" && a != "-c")
            .nth(1)
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let p = std::path::PathBuf::from("dyson.json");
                if p.exists() { Some(p) } else { None }
            });

        let workspace_path = crate::workspace::OpenClawWorkspace::resolve_path(
            Some(settings.workspace.connection_string.expose()),
        );

        let mut reloader = crate::config::hot_reload::HotReloader::new(
            config_path.as_deref(),
            workspace_path.as_deref(),
        );

        eprintln!(
            "Dyson v{} — type /exit to quit",
            env!("CARGO_PKG_VERSION")
        );
        eprintln!();

        loop {
            // Check for config/workspace changes before each turn.
            match reloader.check() {
                Ok((true, new_settings)) => {
                    if let Some(s) = new_settings {
                        current_settings = s;
                        current_settings.dangerous_no_sandbox = settings.dangerous_no_sandbox;
                    }
                    eprintln!("[reloaded]");
                    match super::build_agent(&current_settings, None).await {
                        Ok(a) => {
                            agent = a;
                            current_provider = super::active_provider_name(&current_settings)
                                .unwrap_or_default();
                            current_model = current_settings.agent.model.clone();
                        }
                        Err(e) => eprintln!("[reload error: {e}]"),
                    }
                }
                Ok((false, _)) => {}
                Err(e) => eprintln!("[config reload check failed: {e}]"),
            }

            eprint!("> ");
            std::io::stderr().flush()?;

            let mut input = String::new();
            let bytes_read = std::io::stdin().read_line(&mut input)?;

            if bytes_read == 0 {
                eprintln!();
                break;
            }

            let input = input.trim();
            if input.is_empty() {
                continue;
            }
            if input == "/exit" || input == "/quit" {
                break;
            }

            if input == "/clear" {
                eprintln!("[saving learnings...]");
                agent.save_learnings(&mut output).await;
                agent.clear();
                eprintln!("[context cleared]");
                continue;
            }

            if input == "/compact" {
                eprintln!("[compacting context...]");
                match agent.compact(&mut output).await {
                    Ok(()) => eprintln!("[context compacted]"),
                    Err(e) => eprintln!("[compaction failed: {e}]"),
                }
                continue;
            }

            if input == "/models" {
                if current_settings.providers.is_empty() {
                    eprintln!("No providers configured.");
                } else {
                    eprint!("{}", super::format_provider_list(
                        &current_settings,
                        &current_provider,
                        &current_model,
                    ));
                }
                continue;
            }

            if let Some(args) = input.strip_prefix("/model ").map(str::trim) {
                if args.is_empty() {
                    eprintln!("Usage: /model <provider> [model]  or  /model <model>");
                    continue;
                }
                let (target_provider, target_model) = match super::parse_model_command(
                    args,
                    &current_settings.providers,
                    &current_provider,
                ) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        eprintln!("[{e}]");
                        continue;
                    }
                };
                let messages = agent.messages().to_vec();
                match super::build_agent_with_provider(
                    &current_settings,
                    &target_provider,
                    target_model.as_deref(),
                    None,
                    messages,
                )
                .await
                {
                    Ok(new_agent) => {
                        agent = new_agent;
                        let pc = &current_settings.providers[&target_provider];
                        let resolved = target_model.as_deref()
                            .unwrap_or_else(|| pc.default_model());
                        eprintln!(
                            "[switched to '{}' — {:?} ({})]",
                            target_provider,
                            pc.provider_type,
                            resolved,
                        );
                        current_model = resolved.to_string();
                        current_provider = target_provider;
                    }
                    Err(e) => eprintln!("[switch error: {e}]"),
                }
                continue;
            }
            if input == "/model" {
                eprintln!("Usage: /model <provider> [model]  or  /model <model>");
                continue;
            }

            match agent.run(input, &mut output).await {
                Ok(_) => println!(),
                Err(e) => eprintln!("\n[Error]: {e}"),
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TerminalOutput
// ---------------------------------------------------------------------------

/// Terminal-based output that writes directly to stdout/stderr.
pub struct TerminalOutput {
    stdout: std::io::Stdout,
}

impl TerminalOutput {
    pub fn new() -> Self {
        Self {
            stdout: std::io::stdout(),
        }
    }
}

impl Default for TerminalOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl Output for TerminalOutput {
    fn text_delta(&mut self, text: &str) -> Result<(), DysonError> {
        write!(self.stdout, "{text}")?;
        self.stdout.flush()?;
        Ok(())
    }

    fn tool_use_start(&mut self, _id: &str, name: &str) -> Result<(), DysonError> {
        writeln!(self.stdout, "\n\n[Tool: {name}]")?;
        self.stdout.flush()?;
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<(), DysonError> {
        if output.is_error {
            writeln!(self.stdout, "--- error ---")?;
        } else {
            writeln!(self.stdout, "--- output ---")?;
        }
        writeln!(self.stdout, "{}", output.content)?;
        writeln!(self.stdout, "---")?;
        self.stdout.flush()?;
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<(), DysonError> {
        writeln!(self.stdout, "[File: {}]", path.display())?;
        self.stdout.flush()?;
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        eprintln!("\n[Error]: {error}");
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.stdout.flush()?;
        Ok(())
    }
}
