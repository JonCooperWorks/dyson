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
//   controllers.  You could run both a terminal REPL and a chat bot
//   simultaneously — each as a concurrent task.
// ===========================================================================

use std::io::Write;
use std::path::Path;

use crate::config::Settings;
use crate::controller::{CommandResult, Output, ProviderInfo, ReloadOutcome};
use crate::error::DysonError;
use crate::tool::ToolOutput;

/// Render a `CommandResult` for the terminal.
fn render_command_result_terminal(result: &CommandResult) {
    match result {
        CommandResult::Cleared => eprintln!("[context cleared]"),
        CommandResult::Compacted => eprintln!("[context compacted]"),
        CommandResult::CompactError(e) => eprintln!("[compaction failed: {e}]"),
        CommandResult::ModelList { providers } => {
            if providers.is_empty() {
                eprintln!("No providers configured.");
            } else {
                eprint!("{}", format_provider_list(providers));
            }
        }
        CommandResult::ModelSwitched {
            provider_name,
            provider_type,
            model,
        } => eprintln!("[switched to '{provider_name}' — {provider_type} ({model})]"),
        CommandResult::ModelSwitchError(e) => eprintln!("[switch error: {e}]"),
        CommandResult::ModelParseError(e) => eprintln!("[{e}]"),
        CommandResult::ModelUsage => {
            eprintln!("Usage: /model <provider> [model]  or  /model <model>")
        }
        CommandResult::Logs(lines) => println!("{lines}"),
        CommandResult::LogsError(e) => eprintln!("[logs error: {e}]"),
        CommandResult::LoopStarted { id, chat_id, .. } => {
            eprintln!("[agent #{id} started — chat: {chat_id}]")
        }
        CommandResult::LoopError(e) => eprintln!("[loop error: {e}]"),
        CommandResult::AgentList { agents } => {
            if agents.is_empty() {
                eprintln!("No background agents running.");
            } else {
                eprintln!("Background agents:");
                for a in agents {
                    eprintln!(
                        "  [{}] {} ({:.0}s)",
                        a.id, a.prompt_preview, a.elapsed.as_secs_f64(),
                    );
                }
            }
        }
        CommandResult::AgentStopped { id } => eprintln!("[agent #{id} stopped]"),
        CommandResult::StopError(e) => eprintln!("[stop error: {e}]"),
        CommandResult::NotHandled => {}
    }
}

/// Format a `ProviderInfo` list for terminal display, marking the active model with `*`.
fn format_provider_list(providers: &[ProviderInfo]) -> String {
    let mut out = String::from("Available providers:\n");
    for provider in providers {
        use std::fmt::Write as _;
        writeln!(&mut out, "  {} — {}", provider.name, provider.provider_type).unwrap();
        for model in &provider.models {
            let marker = if model.active { " *" } else { "" };
            writeln!(&mut out, "    {}{marker}", model.name).unwrap();
        }
    }
    out
}

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

    async fn run(
        &self,
        settings: &Settings,
        registry: &std::sync::Arc<super::ClientRegistry>,
    ) -> crate::Result<()> {
        let mut current_settings = settings.clone();

        let client_handle = registry.get_default();
        let mut agent = super::build_agent(
            &current_settings,
            None,
            super::AgentMode::Private,
            client_handle,
            registry,
            None,
        )
        .await?;
        let mut output = TerminalOutput::new();

        let mut current_provider =
            super::active_provider_name(&current_settings).unwrap_or_default();
        let mut current_model = current_settings.agent.model.clone();

        let (config_path, mut reloader) = super::create_hot_reloader(settings);

        let bg_registry = std::sync::Arc::new(super::background::BackgroundAgentRegistry::new());

        eprintln!("Dyson v{} — type /exit to quit", env!("CARGO_PKG_VERSION"));
        eprintln!();

        loop {
            // Check for config/workspace changes before each turn.
            match super::check_and_reload_agent(
                &mut reloader,
                &mut current_settings,
                settings.dangerous_no_sandbox,
                &mut agent,
                &mut current_provider,
                &mut current_model,
                None,
                registry,
            )
            .await
            {
                ReloadOutcome::NoChange => {}
                ReloadOutcome::Reloaded => eprintln!("[reloaded]"),
                ReloadOutcome::Error(e) => eprintln!("[{e}]"),
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

            // Terminal-specific commands.
            if input == "/exit" || input == "/quit" {
                break;
            }

            // Lock-free commands first (no agent needed).  Background agent
            // completions are reported to stderr.
            let on_complete: super::BackgroundCompletion = std::sync::Arc::new(|id, result| {
                eprintln!("\n{}\n", super::format_background_result(id, &result));
            });
            match super::execute_lockfree_command(
                input,
                &current_settings,
                registry,
                &bg_registry,
                Some(on_complete),
            )
            .await
            {
                CommandResult::NotHandled => {}
                result => {
                    render_command_result_terminal(&result);
                    continue;
                }
            }

            // Commands that need the agent.
            match super::execute_agent_command(
                input,
                &mut agent,
                &mut output,
                &current_settings,
                &mut super::ModelState {
                    provider: &mut current_provider,
                    model: &mut current_model,
                    config_path: config_path.as_deref(),
                },
                registry,
            )
            .await
            {
                CommandResult::NotHandled => {}
                result => {
                    render_command_result_terminal(&result);
                    continue;
                }
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

    fn typing_indicator(&mut self, visible: bool) -> Result<(), DysonError> {
        if visible {
            write!(self.stdout, "Typing...")?;
            self.stdout.flush()?;
        } else {
            // Clear the "Typing..." text by overwriting with spaces and
            // returning the cursor to the start of the line.
            write!(self.stdout, "\r          \r")?;
            self.stdout.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.stdout.flush()?;
        Ok(())
    }
}
