use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::sandbox::SandboxDecision;
use crate::skill::local::manifest::{ScriptExecution, resolved_entrypoint};
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::escape_single_quotes;
use dyson_core::error::{DysonError, Result};

const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

pub struct LocalScriptSkillTool {
    name: String,
    skill_name: String,
    description: String,
    skill_dir: PathBuf,
    entrypoint: PathBuf,
    timeout: Duration,
    input_schema: serde_json::Value,
}

impl LocalScriptSkillTool {
    pub fn new(
        tool_name: String,
        skill_name: String,
        description: String,
        skill_dir: PathBuf,
        execution: &ScriptExecution,
    ) -> Result<Self> {
        // Canonical so the directory the sandbox mounts is the one the
        // command names, even when the skills dir sits behind a symlink.
        let skill_dir = std::fs::canonicalize(&skill_dir).unwrap_or(skill_dir);
        let entrypoint = resolved_entrypoint(&skill_dir, &execution.entrypoint)?;
        Ok(Self {
            name: tool_name,
            skill_name,
            description,
            skill_dir,
            entrypoint,
            timeout: Duration::from_millis(execution.timeout_ms),
            input_schema: execution
                .input_schema
                .clone()
                .unwrap_or_else(default_input_schema),
        })
    }
}

impl LocalScriptSkillTool {
    /// The shell command that starts the script, before any sandbox wrap.
    pub(crate) fn script_command(&self) -> String {
        format!(
            "cd '{}' && exec bash '{}'",
            escape_single_quotes(&self.skill_dir.to_string_lossy()),
            escape_single_quotes(&self.entrypoint.to_string_lossy()),
        )
    }

    /// Ask the agent's sandbox how to run the script, the same question a
    /// `bash` call answers.  The agent-loop check on this tool's name is a
    /// network gate only, so without this the script ran unconfined.
    pub(crate) async fn sandboxed_command(
        &self,
        ctx: &ToolContext,
    ) -> std::result::Result<String, String> {
        let Some(sandbox) = ctx.sandbox.as_ref() else {
            return Err(format!(
                "skill '{}' refused: no sandbox is available to run its script",
                self.skill_name
            ));
        };
        let decision = sandbox
            .check_skill_script(&self.script_command(), &self.skill_dir, ctx)
            .await
            .map_err(|e| {
                format!(
                    "skill '{}' refused: sandbox check failed: {e}",
                    self.skill_name
                )
            })?;
        match decision {
            SandboxDecision::Allow { input } => input["command"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "skill '{}' refused: sandbox returned no command",
                        self.skill_name
                    )
                }),
            SandboxDecision::Deny { reason } => Err(format!(
                "skill '{}' denied by sandbox: {reason}",
                self.skill_name
            )),
            SandboxDecision::Redirect { .. } => Err(format!(
                "skill '{}' refused: sandbox redirected the script",
                self.skill_name
            )),
        }
    }
}

#[async_trait]
impl Tool for LocalScriptSkillTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        ignore_sigpipe_for_parent();

        let raw = input
            .get("raw")
            .or_else(|| input.get("input"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let args: Vec<&str> = raw.split_whitespace().collect();
        let payload = json!({
            "raw": raw,
            "args": args,
            "skill": self.skill_name,
            "tool": self.name,
            "workspace": {
                "working_dir": ctx.working_dir.display().to_string(),
                "chat_id": ctx.current_chat_id.as_deref(),
            },
        });

        let command = match self.sandboxed_command(ctx).await {
            Ok(command) => command,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&self.skill_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();

        for (key, value) in std::env::vars() {
            if crate::tool::bash::is_safe_env_var(&key) {
                cmd.env(key, value);
            }
        }
        for (key, value) in &ctx.env {
            if crate::tool::bash::is_safe_env_var(key) {
                cmd.env(key, value);
            }
        }
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| {
            DysonError::tool(
                &self.name,
                format!(
                    "failed to spawn skill script '{}': {e}",
                    self.entrypoint.display()
                ),
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            let bytes = serde_json::to_vec(&payload)
                .map_err(|e| DysonError::tool(&self.name, format!("input encode failed: {e}")))?;
            match stdin.write_all(&bytes).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    tracing::debug!(
                        skill = self.skill_name,
                        "skill script closed stdin before reading input"
                    );
                }
                Err(e) => {
                    return Err(DysonError::tool(
                        &self.name,
                        format!("stdin write failed: {e}"),
                    ));
                }
            }
            let _ = stdin.shutdown().await;
        }

        // Drain both pipes while waiting.  Reading only after `wait()`
        // let a script that wrote more than a pipe buffer block on its
        // write forever, so a fast script was reported as a timeout.
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();
        let outcome = {
            let run = async {
                tokio::join!(
                    child.wait(),
                    read_capped(stdout_handle),
                    read_capped(stderr_handle),
                )
            };
            tokio::select! {
                done = run => RunOutcome::Exited(done),
                _ = tokio::time::sleep(self.timeout) => RunOutcome::TimedOut,
                _ = ctx.cancellation.cancelled() => RunOutcome::Cancelled,
            }
        };

        let (status, stdout, stderr) = match outcome {
            RunOutcome::Exited(done) => done,
            RunOutcome::TimedOut => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ToolOutput::error(format!(
                    "skill '{}' timed out after {} ms",
                    self.skill_name,
                    self.timeout.as_millis()
                )));
            }
            RunOutcome::Cancelled => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(ToolOutput::error(format!(
                    "skill '{}' cancelled",
                    self.skill_name
                )));
            }
        };

        let status =
            status.map_err(|e| DysonError::tool(&self.name, format!("wait failed: {e}")))?;
        let content = format_script_output(&stdout, &stderr);
        if status.success() {
            Ok(ToolOutput::success(content))
        } else {
            Ok(ToolOutput::error(if content.trim().is_empty() {
                format!(
                    "skill '{}' exited with status {}",
                    self.skill_name,
                    status.code().unwrap_or(-1)
                )
            } else {
                content
            }))
        }
    }
}

enum RunOutcome {
    Exited((std::io::Result<std::process::ExitStatus>, Vec<u8>, Vec<u8>)),
    TimedOut,
    Cancelled,
}

/// Keep the first `MAX_READ_BYTES` of a pipe and discard the rest.  The
/// discard matters: stopping at the cap would leave the script blocked on
/// a full pipe again, just later.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(handle: Option<R>) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(mut handle) = handle else {
        return out;
    };
    let _ = AsyncReadExt::read_to_end(&mut (&mut handle).take(MAX_READ_BYTES), &mut out).await;
    let _ = tokio::io::copy(&mut handle, &mut tokio::io::sink()).await;
    out
}

fn ignore_sigpipe_for_parent() {
    #[cfg(unix)]
    {
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        });
    }
}

fn default_input_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "raw": {
                "type": "string",
                "description": "Raw text after the slash command."
            }
        }
    })
}

fn format_script_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim_end().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim_end().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("{stdout}\n\n[stderr]\n{stderr}"),
    }
}
