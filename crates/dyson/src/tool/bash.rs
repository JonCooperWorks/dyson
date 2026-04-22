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

use crate::util::truncate_output;

/// Maximum bytes to read from subprocess stdout/stderr before stopping.
///
/// Prevents a runaway command (e.g. `yes` or an infinite loop printing to
/// stdout) from consuming unbounded memory.  The downstream
/// `truncate_output()` handles further truncation for the LLM context, but
/// this cap protects the process itself from OOM.
///
/// Halved from 10 MB to 5 MB because this applies per stream per command,
/// and the dependency analyzer runs independent tool calls in parallel — a
/// dozen concurrent bash invocations previously peaked at 240 MB
/// (10 MB × 2 streams × 12 commands).  5 MB still comfortably covers large
/// test suites and build outputs before `truncate_output()` shortens them.
const MAX_READ_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

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

    fn agent_only(&self) -> bool {
        true
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
    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let started = std::time::Instant::now();
        // -- Extract the command string --
        let command = input["command"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bash", "missing or invalid 'command' field"))?;

        tracing::info!(command = command, working_dir = %ctx.working_dir.display(), "executing bash command");

        // -- Spawn the child process --
        //
        // Start with a clean environment and selectively add safe variables.
        // This prevents leaking secrets (API keys, tokens) from the parent
        // process environment into commands the LLM can observe via `env`
        // or `printenv`.
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.working_dir)
            .env_clear();

        // Allow-list of safe environment variable prefixes/names to pass through.
        for (key, value) in &ctx.env {
            if is_safe_env_var(key) {
                cmd.env(key, value);
            } else {
                tracing::debug!(key = key.as_str(), "filtering secret env var from bash");
            }
        }

        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DysonError::tool("bash", format!("failed to spawn: {e}")))?;

        // -- Wait with timeout, killing the process if it exceeds the deadline --
        //
        // We take stdout/stderr handles before waiting so we can still
        // kill the child on timeout (wait_with_output takes ownership).
        let mut child = child;
        let mut stdout_handle = child.stdout.take();
        let mut stderr_handle = child.stderr.take();

        let result = tokio::select! {
            status = child.wait() => {
                // Read captured output after the process exits.
                let mut stdout_bytes = Vec::new();
                let mut stderr_bytes = Vec::new();
                if let Some(ref mut h) = stdout_handle
                    && let Err(e) = tokio::io::AsyncReadExt::read_to_end(
                        &mut tokio::io::AsyncReadExt::take(h, MAX_READ_BYTES),
                        &mut stdout_bytes,
                    ).await {
                        tracing::warn!(error = %e, "failed to read bash stdout");
                    }
                if let Some(ref mut h) = stderr_handle
                    && let Err(e) = tokio::io::AsyncReadExt::read_to_end(
                        &mut tokio::io::AsyncReadExt::take(h, MAX_READ_BYTES),
                        &mut stderr_bytes,
                    ).await {
                        tracing::warn!(error = %e, "failed to read bash stderr");
                    }
                Some((status, stdout_bytes, stderr_bytes))
            }
            _ = tokio::time::sleep(self.timeout) => {
                // Timeout expired — kill the child to avoid orphaned processes.
                let _ = child.kill().await;
                let _ = child.wait().await; // reap the zombie
                tracing::warn!(
                    timeout_secs = self.timeout.as_secs(),
                    "bash command timed out — process killed"
                );
                None
            }
        };

        match result {
            // Command completed within the timeout.
            Some((Ok(status), stdout_bytes, stderr_bytes)) => {
                let output = std::process::Output {
                    status,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                };
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                // Build the typed view first while both streams are still
                // borrowed, before `combined` consumes them via into_owned().
                let duration_ms = started.elapsed().as_millis() as u64;
                let view = build_bash_view(
                    command,
                    &stdout,
                    &stderr,
                    output.status.code(),
                    duration_ms,
                );

                // Combine stdout and stderr.  If both are non-empty, label
                // the stderr section so the LLM can distinguish them.
                // Use into_owned() only when needed to avoid cloning
                // valid-UTF8 Cow::Borrowed values.
                let combined = if stderr.is_empty() {
                    stdout.into_owned()
                } else if stdout.is_empty() {
                    stderr.into_owned()
                } else {
                    format!("{stdout}\n--- stderr ---\n{stderr}")
                };

                let truncated = truncate_output(&combined);
                let is_error = !output.status.success();

                tracing::info!(
                    exit_code = output.status.code(),
                    stdout_bytes = output.stdout.len(),
                    stderr_bytes = output.stderr.len(),
                    output_len = truncated.len(),
                    truncated = truncated.len() < combined.len(),
                    is_error = is_error,
                    "bash command completed"
                );

                // Log the first portion of the output for debugging.
                let output_preview = &truncated[..truncated.len().min(300)];
                tracing::debug!(
                    output_preview = output_preview,
                    "bash output preview"
                );

                Ok(ToolOutput {
                    content: truncated.into_owned(),
                    is_error,
                    view: Some(view),
                    metadata: Some(serde_json::json!({
                        "exit_code": output.status.code(),
                        "stdout_bytes": output.stdout.len(),
                        "stderr_bytes": output.stderr.len(),
                    })),
                    files: Vec::new(),
                    checkpoints: Vec::new(),
                    artefacts: Vec::new(),
                })
            }

            // Command completed but wait returned an IO error.
            Some((Err(e), _, _)) => Err(DysonError::tool("bash", format!("process error: {e}"))),

            // Timeout — process was killed above.
            None => Ok(ToolOutput::error(format!(
                "Command timed out after {} seconds and was killed",
                self.timeout.as_secs()
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Environment variable filtering
// ---------------------------------------------------------------------------

/// Check if an environment variable name is safe to pass to child processes.
///
/// Allowlist model: only names that are either on `SAFE_EXACT` or share a
/// prefix in `SAFE_PREFIXES` are passed through.  Anything else (including
/// newly-coined secret names that wouldn't match a blocklist) is
/// filtered out.  This inverts the previous blocklist approach so that
/// leaks fail-closed — a var name that looks innocuous (e.g.
/// `HF_HUB_ACCESS`, `APP_CREDENTIALS_JSON`) no longer slips through just
/// because it doesn't end in `_KEY` / `_TOKEN` / …
///
/// The allowlist is deliberately short.  If a user hits a legitimate var
/// that isn't here, they can run `export VAR=…` in-line in the bash
/// command — the var then lives in the spawned shell, not in Dyson's
/// parent environment.
fn is_safe_env_var(name: &str) -> bool {
    // Exact matches — standard user/shell/locale/tooling vars.
    const SAFE_EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "TMPDIR",
        "TZ",
        "LANG",
        "DISPLAY",
        "EDITOR",
        "VISUAL",
        "PAGER",
        "PWD",
        "OLDPWD",
        // Language toolchain dirs — values, not secrets.
        "CARGO_HOME",
        "RUSTUP_HOME",
        "GOPATH",
        "GOROOT",
        "GOCACHE",
        "GOMODCACHE",
        "VIRTUAL_ENV",
        "PYTHONPATH",
        "NODE_PATH",
        "NPM_CONFIG_PREFIX",
        "JAVA_HOME",
        "MAVEN_HOME",
        "GRADLE_HOME",
        // Common toolchains that need their data dir in the env.
        "PYENV_ROOT",
        "NVM_DIR",
        "RBENV_ROOT",
        // Dyson-internal context (no secrets).
        "DYSON_WORKING_DIR",
    ];
    // Prefix matches — locale variants and terminal/CI signalling.
    const SAFE_PREFIXES: &[&str] = &[
        "LC_",
        "LANGUAGE",
        "COLORTERM",
        "TERM_",
        "XDG_",
        "SSH_CLIENT", // connection metadata, not creds (SSH_AUTH_SOCK is a secret handle)
        "SSH_TTY",
        "GIT_", // GIT_DIR, GIT_INDEX_FILE, etc. — note GIT_ASKPASS is a path not a secret
    ];

    // Env names are case-sensitive on Unix (the only platform Dyson
    // supports).  Match exactly so lowercased variants like `path` or
    // `lc_all` don't sneak past the allowlist on the grounds of an
    // accidental case-insensitive compare.
    if SAFE_EXACT.contains(&name) {
        return true;
    }
    if SAFE_PREFIXES
        .iter()
        .any(|&s| name.len() >= s.len() && &name[..s.len()] == s)
    {
        return true;
    }

    false
}

/// Build the typed terminal view for the HTTP controller.
///
/// Lines are classified `'p'` for the prompt, `'c'` for stdout, `'e'` for
/// stderr, `'d'` for dim/ancillary.  Truncates each stream to a sane line
/// count so the SSE payload stays small (full output still goes to the LLM).
fn build_bash_view(
    command: &str,
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    duration_ms: u64,
) -> crate::tool::view::ToolView {
    use crate::tool::view::{TermLine, ToolView};
    const MAX_LINES_PER_STREAM: usize = 200;
    let mut lines: Vec<TermLine> = Vec::new();
    lines.push(TermLine {
        c: 'p',
        t: format!("$ {command}"),
    });
    let mut take_lines = |s: &str, c: char| {
        for (i, l) in s.lines().enumerate() {
            if i == MAX_LINES_PER_STREAM {
                lines.push(TermLine {
                    c: 'd',
                    t: format!("… ({} more lines)", s.lines().count() - i),
                });
                break;
            }
            lines.push(TermLine {
                c,
                t: l.to_string(),
            });
        }
    };
    take_lines(stdout, 'c');
    take_lines(stderr, 'e');
    ToolView::Bash {
        lines,
        exit_code,
        duration_ms,
    }
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
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert_eq!(output.content.trim(), "hello");
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let tool = BashTool::default();
        let input = serde_json::json!({"command": "false"});
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn captures_stderr() {
        let tool = BashTool::default();
        let input = serde_json::json!({"command": "echo oops >&2"});
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert!(output.content.contains("oops"));
    }

    #[tokio::test]
    async fn missing_command_field() {
        let tool = BashTool::default();
        let input = serde_json::json!({"wrong_field": "ls"});
        let result = tool.run(&input, &test_ctx()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn timeout_returns_error_output() {
        let tool = BashTool {
            timeout: Duration::from_millis(100),
        };
        let input = serde_json::json!({"command": "sleep 10"});
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("timed out"));
    }

    #[test]
    fn env_allowlist_passes_system_vars() {
        for v in ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "USER"] {
            assert!(is_safe_env_var(v), "expected {v} to be safe");
        }
    }

    #[test]
    fn env_allowlist_blocks_unknown_names() {
        // The previous blocklist missed names like these because they
        // don't match `_KEY`/`_TOKEN`/etc. suffix patterns.
        for v in [
            "HF_HUB_ACCESS",
            "APP_CREDENTIALS_JSON",
            "MY_API_AUTH",
            "STRIPE_WEBHOOK",
            "DATABASE_URL",
            "AWS_ACCESS_KEY_ID",
            "OPENAI_API_KEY",
            "SSH_AUTH_SOCK", // explicitly not in the allowlist
        ] {
            assert!(!is_safe_env_var(v), "expected {v} to be filtered");
        }
    }
}
