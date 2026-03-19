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

use crate::agent::Agent;
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
        let client = crate::llm::create_client(&settings.agent);
        let sandbox = crate::sandbox::create_sandbox(
            &settings.sandbox,
            settings.dangerous_no_sandbox,
        );
        let skills = crate::skill::create_skills(settings).await;
        let mut agent = Agent::new(client, sandbox, skills, &settings.agent)?;
        let mut output = TerminalOutput::new();

        eprintln!(
            "Dyson v{} — type /exit to quit",
            env!("CARGO_PKG_VERSION")
        );
        eprintln!();

        loop {
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

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        eprintln!("\n[Error]: {error}");
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.stdout.flush()?;
        Ok(())
    }
}
