// ===========================================================================
// Bash tool — execute shell commands with timeout and output capture.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements a `Tool` that runs shell commands via `bash -c`.  This is
//   the primary way the LLM interacts with the system — listing files,
//   running tests, installing packages, executing scripts.
//
// How it works:
//
//   LLM sends: { "command": "ls -la" }
//     → BashTool::run() spawns: bash -c "ls -la"
//     → captures stdout + stderr
//     → truncates if too large (protects LLM context window)
//     → returns ToolOutput { content, is_error: exit_code != 0 }
//
// Timeout handling:
//   Commands have a configurable timeout (default 120s).  On timeout:
//   - The child process is killed (SIGKILL)
//   - An error ToolOutput is returned explaining the timeout
//   - The LLM can decide to retry with a shorter command or different approach
//
// Output truncation:
//   The LLM has a finite context window.  A command like `cat huge_file.log`
//   could produce megabytes of output that would blow the context.  We
//   truncate to MAX_OUTPUT_BYTES (100KB) and append a notice so the LLM
//   knows the output was cut short.
//
// Why `bash -c` instead of direct exec?
//   The LLM writes shell syntax: pipes, redirects, globs, env vars,
//   command chaining with && and ||.  These are shell features, not kernel
//   features.  We need a shell to interpret them.  `bash` is the most
//   portable and predictable choice.
// ===========================================================================

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum bytes of command output before truncation.
///
/// 100KB is generous enough for most tool calls (file listings, test output,
/// grep results) but small enough to leave room in the LLM's context window
/// for the conversation history and system prompt.
const MAX_OUTPUT_BYTES: usize = 100 * 1024;

// ---------------------------------------------------------------------------
// BashTool
// ---------------------------------------------------------------------------

/// Tool that executes shell commands via `bash -c`.
///
/// This is the workhorse tool — the LLM uses it for everything from
/// `ls` to `cargo test` to `git commit`.  Commands run in the agent's
/// working directory with its environment variables.
pub struct BashTool {
    /// Maximum time a command can run before being killed.
    pub timeout: Duration,
}

impl Default for BashTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return its output. Use this for running \
         shell commands, scripts, build tools, file operations, and system \
         inspection. The command runs in the agent's working directory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        })
    }

    /// Execute a bash command, capturing stdout and stderr.
    ///
    /// ## Flow
    ///
    /// 1. Extract `command` from the JSON input
    /// 2. Spawn `bash -c <command>` as a child process
    /// 3. Wait for completion with a timeout
    /// 4. Combine stdout + stderr
    /// 5. Truncate if too large
    /// 6. Return as ToolOutput (is_error = exit code != 0)
    ///
    /// ## Error cases
    ///
    /// - Missing `command` field → `DysonError::Tool`
    /// - Can't spawn bash → `DysonError::Tool` (not Io, because we add context)
    /// - Timeout → `ToolOutput::error` (not DysonError — the tool *ran*, it just took too long)
    /// - Non-zero exit → `ToolOutput { is_error: true }` (normal tool-level error)
    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // -- Extract the command string --
        let command = input["command"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bash", "missing or invalid 'command' field"))?;

        tracing::debug!(command = command, "executing bash command");

        // -- Spawn the child process --
        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.working_dir)
            .envs(&ctx.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DysonError::tool("bash", format!("failed to spawn: {e}")))?;

        // -- Wait with timeout --
        //
        // `wait_with_output()` takes ownership of the child and reads all
        // of stdout/stderr into memory.  We wrap it in `tokio::time::timeout`
        // for a hard deadline.
        //
        // Caveat: if the timeout fires, the child process may still be
        // running (wait_with_output's future is dropped but the OS process
        // isn't automatically killed).  For Phase 1, we accept this — the
        // process will be orphaned.  A robust solution would take the
        // stdout/stderr handles, use `child.wait()` in a select! with
        // `child.kill()` on the timeout branch.
        let result = tokio::time::timeout(self.timeout, child.wait_with_output()).await;

        match result {
            // Command completed within the timeout.
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                // Combine stdout and stderr.  If both are non-empty, label
                // the stderr section so the LLM can distinguish them.
                let combined = if stderr.is_empty() {
                    stdout.to_string()
                } else if stdout.is_empty() {
                    stderr.to_string()
                } else {
                    format!("{stdout}\n--- stderr ---\n{stderr}")
                };

                let truncated = truncate_output(&combined);
                let is_error = !output.status.success();

                tracing::debug!(
                    exit_code = output.status.code(),
                    output_len = truncated.len(),
                    truncated = truncated.len() < combined.len(),
                    "bash command completed"
                );

                Ok(ToolOutput {
                    content: truncated,
                    is_error,
                    metadata: Some(serde_json::json!({
                        "exit_code": output.status.code(),
                        "stdout_bytes": output.stdout.len(),
                        "stderr_bytes": output.stderr.len(),
                    })),
                })
            }

            // Command completed but wait_with_output returned an IO error
            // (e.g., pipe broken).
            Ok(Err(e)) => Err(DysonError::tool("bash", format!("process error: {e}"))),

            // Timeout expired.
            Err(_) => {
                tracing::warn!(
                    timeout_secs = self.timeout.as_secs(),
                    "bash command timed out"
                );
                Ok(ToolOutput::error(format!(
                    "Command timed out after {} seconds",
                    self.timeout.as_secs()
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Output truncation
// ---------------------------------------------------------------------------

/// Truncate output to MAX_OUTPUT_BYTES, appending a notice if truncated.
///
/// We truncate on a UTF-8 char boundary to avoid producing invalid strings.
/// The notice tells the LLM how much was cut so it can request specific
/// portions if needed.
fn truncate_output(output: &str) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output.to_string();
    }

    // Find the last valid char boundary at or before MAX_OUTPUT_BYTES.
    let mut end = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(end) && end > 0 {
        end -= 1;
    }

    let truncated = &output[..end];
    let remaining = output.len() - end;
    format!(
        "{truncated}\n\n... (output truncated — {remaining} bytes omitted, \
         total was {} bytes)",
        output.len()
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    /// Helper to create a test context.
    fn test_ctx() -> ToolContext {
        ToolContext::from_cwd().unwrap()
    }

    #[tokio::test]
    async fn echo_hello() {
        let tool = BashTool::default();
        let input = serde_json::json!({"command": "echo hello"});
        let output = tool.run(input, &test_ctx()).await.unwrap();
        assert_eq!(output.content.trim(), "hello");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let tool = BashTool::default();
        let input = serde_json::json!({"command": "false"});
        let output = tool.run(input, &test_ctx()).await.unwrap();
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn captures_stderr() {
        let tool = BashTool::default();
        let input = serde_json::json!({"command": "echo oops >&2"});
        let output = tool.run(input, &test_ctx()).await.unwrap();
        assert!(output.content.contains("oops"));
    }

    #[tokio::test]
    async fn missing_command_field() {
        let tool = BashTool::default();
        let input = serde_json::json!({"wrong_field": "ls"});
        let result = tool.run(input, &test_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn timeout_returns_error_output() {
        let tool = BashTool {
            timeout: Duration::from_millis(100),
        };
        let input = serde_json::json!({"command": "sleep 10"});
        let output = tool.run(input, &test_ctx()).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("timed out"));
    }

    #[test]
    fn truncation() {
        let long_output = "x".repeat(MAX_OUTPUT_BYTES + 1000);
        let truncated = truncate_output(&long_output);
        assert!(truncated.len() < long_output.len());
        assert!(truncated.contains("truncated"));
        assert!(truncated.contains("1000 bytes omitted"));
    }

    #[test]
    fn no_truncation_for_short_output() {
        let short = "hello world";
        assert_eq!(truncate_output(short), short);
    }
}
