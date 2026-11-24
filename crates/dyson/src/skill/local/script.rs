use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Once;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::skill::local::manifest::{ScriptExecution, resolved_entrypoint};
use crate::tool::{Tool, ToolContext, ToolOutput};
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

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg(&self.entrypoint)
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
