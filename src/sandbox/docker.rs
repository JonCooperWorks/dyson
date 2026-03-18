// ===========================================================================
// Docker sandbox — route tool calls through a container.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the Sandbox trait to intercept tool calls and execute them
//   inside a Docker container instead of on the host.  The LLM thinks it's
//   running `ls` on your machine — it's actually running inside an isolated
//   container with limited filesystem, network, and privilege access.
//
// How it works:
//
//   Without sandbox:
//     LLM: bash {"command": "cat /etc/passwd"}
//       → BashTool runs on host
//       → returns your actual /etc/passwd
//
//   With DockerSandbox:
//     LLM: bash {"command": "cat /etc/passwd"}
//       → check() rewrites to: docker exec <container> bash -c 'cat /etc/passwd'
//       → BashTool runs the docker exec on host
//       → returns the container's /etc/passwd (not yours)
//
//   The rewrite happens in check().  The BashTool doesn't know it's
//   running a docker command — it just executes whatever command it gets.
//   The LLM doesn't know either — it gets back a normal tool_result.
//
// Container lifecycle:
//   The DockerSandbox can operate in two modes:
//
//   1. Pre-existing container — you start a container yourself, pass
//      its name/ID to the sandbox.  The sandbox just runs docker exec.
//
//   2. Managed container — the sandbox starts a container on init and
//      stops it on drop.  (Phase 1 only implements pre-existing.)
//
// What gets sandboxed:
//
//   | Tool | Sandboxed? | How |
//   |------|-----------|-----|
//   | bash | Yes | Command rewritten to `docker exec` |
//   | read_file | Future | Path remapped to container mount |
//   | write_file | Future | Path remapped to container mount |
//   | MCP tools | No | MCP calls are network I/O, not host access |
//
// Security properties:
//
//   The container provides:
//   - Filesystem isolation — can't read host files outside mounts
//   - Process isolation — can't see or kill host processes
//   - Network isolation (if configured) — can't access host network
//   - Resource limits — can't exhaust host memory/CPU
//   - No privilege escalation — runs as non-root (if image configured)
//
//   The container does NOT protect against:
//   - Docker escape vulnerabilities (rare but exist)
//   - Mounted volumes (the working dir is typically mounted)
//   - Network access (unless --network=none)
//   - The docker socket (never mount /var/run/docker.sock)
//
// Configuration (in dyson.json):
//
//   ```json
//   {
//     "sandbox": {
//       "type": "docker",
//       "image": "ubuntu:24.04",
//       "mounts": ["/path/to/project:/workspace"],
//       "network": "none",
//       "memory": "512m",
//       "cpus": "1"
//     }
//   }
//   ```
//
// Shell escaping:
//   The LLM's command is passed to `docker exec <container> bash -c '<cmd>'`.
//   Single quotes in the command are escaped as `'\''` (end quote, literal
//   quote, start quote) — the standard shell escaping trick.  This prevents
//   injection where the LLM crafts a command that breaks out of the quotes.
// ===========================================================================

use async_trait::async_trait;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::{ToolContext, ToolOutput};

// ---------------------------------------------------------------------------
// DockerSandbox
// ---------------------------------------------------------------------------

/// Sandbox that routes bash commands through a Docker container.
///
/// Every bash tool call is rewritten from:
///   `bash -c "ls -la"`
/// To:
///   `docker exec <container> bash -c 'ls -la'`
///
/// The LLM and the BashTool are both unaware of the rewrite.
pub struct DockerSandbox {
    /// Docker container name or ID to exec into.
    ///
    /// The container must already be running.  Start it with:
    /// ```bash
    /// docker run -d --name dyson-sandbox \
    ///   -v $(pwd):/workspace \
    ///   -w /workspace \
    ///   --network none \
    ///   --memory 512m \
    ///   ubuntu:24.04 sleep infinity
    /// ```
    pub container: String,
}

impl DockerSandbox {
    pub fn new(container: &str) -> Self {
        Self {
            container: container.to_string(),
        }
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    /// Rewrite bash commands to run inside the Docker container.
    ///
    /// ## Flow
    ///
    /// 1. If the tool is "bash", rewrite the command:
    ///    - Extract the "command" field from the input
    ///    - Escape single quotes in the command
    ///    - Wrap in: `docker exec <container> bash -c '<escaped_command>'`
    ///    - Return Allow with the rewritten input
    ///
    /// 2. All other tools pass through unchanged.
    ///    MCP tools, for example, are network calls that don't need
    ///    containerization.
    ///
    /// ## Why Allow + rewrite instead of Redirect?
    ///
    /// We rewrite the command but keep using the same BashTool.  The
    /// BashTool doesn't care what command it runs — `ls` and `docker exec
    /// ... ls` are both just strings to `bash -c`.  No need for a separate
    /// DockerBashTool.
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        match tool_name {
            "bash" => {
                let command = input["command"].as_str().unwrap_or("");

                if command.is_empty() {
                    return Ok(SandboxDecision::Allow {
                        input: input.clone(),
                    });
                }

                // Escape single quotes for safe embedding in bash -c '...'.
                //
                // The trick: replace every ' with '\'' which:
                //   1. Ends the current single-quoted string
                //   2. Adds a literal ' via \'
                //   3. Starts a new single-quoted string
                //
                // Example: "it's here" → "it'\''s here"
                // In shell: bash -c 'it'\''s here'
                let escaped = command.replace('\'', "'\\''");

                let docker_cmd = format!(
                    "docker exec {} bash -c '{}'",
                    self.container, escaped
                );

                tracing::debug!(
                    original = command,
                    rewritten = docker_cmd,
                    container = self.container,
                    "bash command routed to container"
                );

                Ok(SandboxDecision::Allow {
                    input: serde_json::json!({ "command": docker_cmd }),
                })
            }

            // All other tools pass through unchanged.
            _ => Ok(SandboxDecision::Allow {
                input: input.clone(),
            }),
        }
    }

    /// Post-process tool output from the container.
    ///
    /// Currently a no-op.  Future uses:
    /// - Strip container-internal paths from output
    /// - Redact secrets that leaked into command output
    /// - Add audit metadata (which container, timestamp)
    /// - Enforce output size limits stricter than the default
    async fn after(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _output: &mut ToolOutput,
    ) -> Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn rewrites_bash_commands() {
        let sandbox = DockerSandbox::new("my-container");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "ls -la"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let cmd = input["command"].as_str().unwrap();
                assert!(cmd.starts_with("docker exec my-container bash -c"));
                assert!(cmd.contains("ls -la"));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn escapes_single_quotes() {
        let sandbox = DockerSandbox::new("test-box");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "echo 'hello world'"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let cmd = input["command"].as_str().unwrap();
                // The single quotes in the original command should be escaped
                // so they don't break the outer bash -c '...' wrapper.
                assert!(cmd.contains("'\\''"));
                assert!(cmd.starts_with("docker exec test-box bash -c"));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_bash_tools_pass_through() {
        let sandbox = DockerSandbox::new("test-box");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"query": "search something"});

        let decision = sandbox
            .check("resolve-library-id", &input, &ctx)
            .await
            .unwrap();
        match decision {
            SandboxDecision::Allow { input: allowed } => {
                assert_eq!(allowed["query"], "search something");
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_command_passes_through() {
        let sandbox = DockerSandbox::new("test-box");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": ""});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input: allowed } => {
                // Empty command should pass through, not be wrapped in docker exec.
                assert_eq!(allowed["command"], "");
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }
}
