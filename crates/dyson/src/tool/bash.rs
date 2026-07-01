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
//   - The child process group is killed (SIGKILL on Unix)
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
use crate::sandbox::policy_sandbox::redact_secrets;
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
        let command = crate::tool::required_str(input, "command", "bash")?;

        let log_command = redact_secrets(command);
        tracing::info!(command = %log_command, working_dir = %ctx.working_dir.display(), "executing bash command");

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
        configure_process_group(&mut cmd);

        // Source from the process env first so the spawned shell at least
        // has PATH, HTTPS_PROXY, etc. — the parent dyson process inherits
        // these from the cube image, and without them every shell command
        // runs without a $PATH and curl has no proxy URL to dial.
        // Then layer ctx.env on top so callers can override or add per-
        // invocation overrides (mostly tests).  Allow-list filter applies
        // identically to both sources so secrets in either layer are
        // dropped.
        for (key, value) in std::env::vars() {
            if is_safe_env_var(&key) {
                cmd.env(key, value);
            }
        }
        for (key, value) in &ctx.env {
            if is_safe_env_var(key) {
                cmd.env(key, value);
            } else {
                tracing::debug!(key = key.as_str(), "filtering secret env var from bash");
            }
        }

        // `ManagedChild` is the backstop: if this future is dropped mid-run
        // (turn cancelled via `tokio::select!` in the controller), the whole
        // shell process group gets SIGKILLed instead of leaving grandchildren
        // behind.
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DysonError::tool("bash", format!("failed to spawn: {e}")))?;
        let mut child = ManagedChild::new(child);

        // -- Drain stdout/stderr concurrently with wait() --
        //
        // Reading only after the child exits deadlocks any command whose
        // output exceeds one pipe buffer (~64 KB): the child blocks on
        // write(2), wait() never resolves, and the timeout kills it with
        // all output discarded.  Each drain task captures up to
        // MAX_READ_BYTES and then discards the excess so the pipe keeps
        // flowing no matter how much the command prints.
        let stdout_task = tokio::spawn(drain_capped(child.stdout_mut().take()));
        let stderr_task = tokio::spawn(drain_capped(child.stderr_mut().take()));

        enum WaitOutcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }

        let outcome = tokio::select! {
            status = child.wait() => WaitOutcome::Exited(status),
            _ = tokio::time::sleep(self.timeout) => {
                // Timeout expired — kill the child to avoid orphaned
                // processes.  On Unix this kills the whole process group.
                let _ = child.kill().await;
                tracing::warn!(
                    timeout_secs = self.timeout.as_secs(),
                    "bash command timed out — process killed"
                );
                WaitOutcome::TimedOut
            }
            _ = ctx.cancellation.cancelled() => {
                let _ = child.kill().await;
                tracing::info!("bash command cancelled — process killed");
                WaitOutcome::Cancelled
            }
        };

        // The pipes hit EOF once the child is dead, so these resolve
        // promptly on every path — including timeout and cancellation,
        // where the partial output is still available below.
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();

        match outcome {
            // Command completed within the timeout.
            WaitOutcome::Exited(Ok(status)) => {
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
                let view =
                    build_bash_view(command, &stdout, &stderr, output.status.code(), duration_ms);

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

                tracing::debug!(output_len = truncated.len(), "bash output captured");

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
            WaitOutcome::Exited(Err(e)) => {
                Err(DysonError::tool("bash", format!("process error: {e}")))
            }

            // Timeout — process was killed above; return what it managed
            // to print so the LLM can diagnose instead of flying blind.
            WaitOutcome::TimedOut => {
                let mut msg = format!(
                    "Command timed out after {} seconds and was killed",
                    self.timeout.as_secs()
                );
                append_partial_output(&mut msg, &stdout_bytes, &stderr_bytes);
                Ok(ToolOutput::error(msg))
            }

            // Cancelled — process was killed above.
            WaitOutcome::Cancelled => {
                let mut msg = "Command cancelled — process killed".to_string();
                append_partial_output(&mut msg, &stdout_bytes, &stderr_bytes);
                Ok(ToolOutput::error(msg))
            }
        }
    }
}

#[cfg(unix)]
fn configure_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_cmd: &mut tokio::process::Command) {}

struct ManagedChild {
    child: tokio::process::Child,
    #[cfg(unix)]
    pgid: Option<i32>,
    finished: bool,
}

impl ManagedChild {
    fn new(child: tokio::process::Child) -> Self {
        #[cfg(unix)]
        let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
        Self {
            child,
            #[cfg(unix)]
            pgid,
            finished: false,
        }
    }

    fn stdout_mut(&mut self) -> &mut Option<tokio::process::ChildStdout> {
        &mut self.child.stdout
    }

    fn stderr_mut(&mut self) -> &mut Option<tokio::process::ChildStderr> {
        &mut self.child.stderr
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.finished = true;
        }
        status
    }

    async fn kill(&mut self) -> std::io::Result<()> {
        self.kill_group();
        let result = self.child.kill().await;
        self.finished = true;
        result
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // Negative PID targets the process group.  Ignore ESRCH: the
            // shell may have already exited between select wake and kill.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.kill_group();
        let _ = self.child.start_kill();
    }
}

/// Read a child output stream to completion: capture up to
/// [`MAX_READ_BYTES`], then keep reading and discarding so the child never
/// blocks on a full pipe.  Returns the captured prefix.
async fn drain_capped<R>(reader: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut captured = Vec::new();
    if let Err(e) = (&mut reader)
        .take(MAX_READ_BYTES)
        .read_to_end(&mut captured)
        .await
    {
        tracing::warn!(error = %e, "failed to read bash output stream");
        return captured;
    }
    let mut sink = [0u8; 8192];
    loop {
        match reader.read(&mut sink).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    captured
}

/// Append whatever the command printed before it was killed (timeout or
/// cancellation) to the error message, truncated for the LLM context.
fn append_partial_output(msg: &mut String, stdout_bytes: &[u8], stderr_bytes: &[u8]) {
    let stdout = String::from_utf8_lossy(stdout_bytes);
    let stderr = String::from_utf8_lossy(stderr_bytes);
    let combined = if stderr.trim().is_empty() {
        stdout.into_owned()
    } else if stdout.trim().is_empty() {
        stderr.into_owned()
    } else {
        format!("{stdout}\n--- stderr ---\n{stderr}")
    };
    if !combined.trim().is_empty() {
        msg.push_str("\n--- partial output before the process was killed ---\n");
        msg.push_str(&truncate_output(&combined));
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
pub(crate) fn is_safe_env_var(name: &str) -> bool {
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
        // HTTP forward-proxy configuration.  Values, not secrets — the
        // proxy URL just tells `curl` / `wget` / language SDKs which
        // host to dial when stepping outbound.  Required when the
        // agent runs in a sandbox where direct egress doesn't reach
        // every destination (some upstream networks drop SYN-ACKs for
        // the kernel-bypass NAT path the cube uses); without these
        // forwarded into the spawned shell, `curl https://google.com`
        // dials Google directly and silently times out even though
        // the cube image bakes in a working proxy.
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
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
        t: format!("$ {}", redact_secrets(command)),
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

    // Regression: stdout/stderr used to be read only AFTER child.wait()
    // resolved.  A child emitting more than one pipe buffer (~64 KB)
    // blocked on write(2) forever, wait() never returned, and the
    // timeout killed the command and discarded all output.  The streams
    // must be drained concurrently with wait().
    #[tokio::test]
    async fn large_output_does_not_deadlock() {
        let tool = BashTool {
            timeout: Duration::from_secs(10),
        };
        // ~1 MB to stdout — far larger than any pipe buffer.
        let input = serde_json::json!({"command": "head -c 1000000 /dev/zero | tr '\\0' 'x'"});
        let started = std::time::Instant::now();
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert!(
            !output.is_error,
            "1 MB of output must not deadlock into the timeout: {}",
            output.content.chars().take(200).collect::<String>()
        );
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "should complete promptly, not ride the timeout"
        );
        let meta = output.metadata.expect("bash metadata");
        assert!(
            meta["stdout_bytes"].as_u64().unwrap() >= 1_000_000,
            "full stdout should have been drained: {meta}"
        );
    }

    // A stream larger than MAX_READ_BYTES must still terminate (excess
    // is discarded, capped capture returned) instead of blocking the
    // child on a full pipe.
    #[tokio::test]
    async fn output_beyond_cap_is_truncated_not_deadlocked() {
        let tool = BashTool {
            timeout: Duration::from_secs(30),
        };
        // 6 MB > MAX_READ_BYTES (5 MB).
        let input = serde_json::json!({"command": "head -c 6000000 /dev/zero | tr '\\0' 'y'"});
        let output = tool.run(&input, &test_ctx()).await.unwrap();
        assert!(!output.is_error, "oversized output must not time out");
        let meta = output.metadata.expect("bash metadata");
        let captured = meta["stdout_bytes"].as_u64().unwrap();
        assert!(
            captured <= MAX_READ_BYTES,
            "capture must be capped at MAX_READ_BYTES, got {captured}"
        );
        assert!(captured >= MAX_READ_BYTES / 2, "cap-sized prefix expected");
    }

    // Cancelling the turn must kill the child process, not orphan it.
    #[tokio::test]
    async fn cancellation_kills_child_process() {
        let tool = BashTool::default(); // 120s timeout — cancellation must beat it
        let mut ctx = test_ctx();
        let token = tokio_util::sync::CancellationToken::new();
        ctx.cancellation = token.clone();
        let pidfile = std::env::temp_dir().join(format!(
            "dyson_bash_cancel_pid_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&pidfile);
        let cmd = format!("echo $$ > '{}'; sleep 30", pidfile.display());
        let input = serde_json::json!({ "command": cmd });

        let task = tokio::spawn(async move { tool.run(&input, &ctx).await });
        // Give the child time to start and write its pidfile.
        for _ in 0..50 {
            if pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        token.cancel();

        let output = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancelled bash run must return promptly, not ride the timeout")
            .unwrap()
            .unwrap();
        assert!(output.is_error, "cancellation should surface as an error");

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("child should have written its pid before cancel")
            .trim()
            .parse()
            .unwrap();
        // The child was SIGKILLed and reaped — its /proc entry must be gone
        // (allow a beat for the kernel).
        let mut alive = true;
        for _ in 0..50 {
            alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "child pid {pid} must be killed on cancellation");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_grandchild_process_group() {
        let tool = BashTool::default();
        let mut ctx = test_ctx();
        let token = tokio_util::sync::CancellationToken::new();
        ctx.cancellation = token.clone();
        let pidfile = std::env::temp_dir().join(format!(
            "dyson_bash_cancel_grandchild_pid_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&pidfile);
        let cmd = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let input = serde_json::json!({ "command": cmd });

        let task = tokio::spawn(async move { tool.run(&input, &ctx).await });
        for _ in 0..50 {
            if pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        token.cancel();

        let output = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancelled bash run must return promptly")
            .unwrap()
            .unwrap();
        assert!(output.is_error, "cancellation should surface as an error");

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("grandchild should have written its pid before cancel")
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !alive,
            "grandchild pid {pid} must be killed on cancellation"
        );
        let _ = std::fs::remove_file(&pidfile);
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

    // M10: the SSE `view` channel used to ship the raw command line. The
    // text log already runs through redact_secrets, so the view path is
    // a credential exfil channel by parity. The "$" prompt line must
    // carry the redacted form for any bearer-shaped argument.
    #[test]
    fn bash_view_prompt_line_is_redacted() {
        let view = build_bash_view(
            "curl -H 'Authorization: Bearer sk-deadbeefdeadbeef' https://api.example.com",
            "ok\n",
            "",
            Some(0),
            12,
        );
        let crate::tool::view::ToolView::Bash { lines, .. } = view else {
            panic!("expected Bash view");
        };
        let prompt = lines.iter().find(|l| l.c == 'p').expect("prompt line");
        assert!(
            !prompt.t.contains("sk-deadbeefdeadbeef"),
            "bearer must not appear in view prompt: {:?}",
            prompt.t
        );
    }
}
