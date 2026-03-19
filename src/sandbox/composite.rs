// ===========================================================================
// Composite sandbox — chain multiple sandboxes into a pipeline.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements a Sandbox that runs multiple inner sandboxes in sequence.
//   Each sandbox gets a turn to inspect the tool call.  First Deny wins.
//   Allow passes the (possibly rewritten) input to the next sandbox.
//   Redirect short-circuits like Deny.
//
// Why composition instead of one big sandbox?
//   A single sandbox that handles bash, files, network, and audit is a
//   mess.  Each concern is different:
//
//   - DockerSandbox knows about bash commands and containers
//   - FileSandbox knows about path restrictions
//   - NetworkSandbox knows about URL whitelists
//   - AuditSandbox knows about logging
//
//   None of them should know about each other.  Composition lets you
//   build a security policy from independent, focused pieces.
//
// How the pipeline works:
//
//   CompositeSandbox([AuditSandbox, FileSandbox, DockerSandbox])
//
//   Tool call: bash {"command": "cat /etc/shadow"}
//     │
//     ▼
//   AuditSandbox.check("bash", {"command": "cat /etc/shadow"})
//     → Allow { input unchanged }   (audit logs it, doesn't block)
//     │
//     ▼
//   FileSandbox.check("bash", {"command": "cat /etc/shadow"})
//     → Deny { reason: "/etc/shadow is restricted" }
//     │
//     ▼
//   STOP — first Deny wins.  DockerSandbox never runs.
//   Agent receives: ToolOutput::error("Denied: /etc/shadow is restricted")
//
// Another example — rewrite chaining:
//
//   CompositeSandbox([AuditSandbox, DockerSandbox])
//
//   Tool call: bash {"command": "ls"}
//     │
//     ▼
//   AuditSandbox.check("bash", {"command": "ls"})
//     → Allow { input unchanged }
//     │
//     ▼
//   DockerSandbox.check("bash", {"command": "ls"})
//     → Allow { input: {"command": "docker exec sandbox bash -c 'ls'"} }
//     │
//     ▼
//   Final decision: Allow with rewritten input.
//   BashTool runs: docker exec sandbox bash -c 'ls'
//
// The `after()` pipeline:
//   After the tool executes, each sandbox's `after()` runs in the SAME
//   order.  Each can inspect and mutate the output.  This lets AuditSandbox
//   log the final output after DockerSandbox has had a chance to clean it.
//
// Order matters:
//   Put sandboxes that DENY first (fail fast).
//   Put sandboxes that REWRITE in the middle.
//   Put sandboxes that OBSERVE (audit) first or last depending on whether
//   you want to log the original or rewritten input.
// ===========================================================================

use async_trait::async_trait;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::{ToolContext, ToolOutput};

// ---------------------------------------------------------------------------
// CompositeSandbox
// ---------------------------------------------------------------------------

/// Chains multiple sandboxes into a sequential pipeline.
///
/// Each sandbox gets a turn.  The pipeline short-circuits on the first
/// `Deny` or `Redirect`.  `Allow` passes the (possibly rewritten) input
/// to the next sandbox in the chain.
///
/// ## Construction
///
/// ```ignore
/// let sandbox = CompositeSandbox::new(vec![
///     Box::new(AuditSandbox::new("audit.log")),
///     Box::new(FileSandbox::new(vec!["/etc", "/root"])),
///     Box::new(DockerSandbox::new("dyson-sandbox")),
/// ]);
/// ```
pub struct CompositeSandbox {
    /// Inner sandboxes, executed in order.
    sandboxes: Vec<Box<dyn Sandbox>>,
}

impl CompositeSandbox {
    pub fn new(sandboxes: Vec<Box<dyn Sandbox>>) -> Self {
        Self { sandboxes }
    }
}

#[async_trait]
impl Sandbox for CompositeSandbox {
    /// Run each sandbox's check() in sequence.
    ///
    /// - `Deny` → stop immediately, return the denial
    /// - `Redirect` → stop immediately, return the redirect
    /// - `Allow { input }` → pass the (possibly rewritten) input to the
    ///   next sandbox
    ///
    /// If all sandboxes allow, return the final (possibly multiply-rewritten)
    /// input.
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        let mut current_input = input.clone();

        for sandbox in &self.sandboxes {
            match sandbox.check(tool_name, &current_input, ctx).await? {
                SandboxDecision::Deny { reason } => {
                    return Ok(SandboxDecision::Deny { reason });
                }
                SandboxDecision::Redirect { tool_name, input } => {
                    return Ok(SandboxDecision::Redirect { tool_name, input });
                }
                SandboxDecision::Allow { input } => {
                    current_input = input;
                }
            }
        }

        Ok(SandboxDecision::Allow {
            input: current_input,
        })
    }

    /// Run each sandbox's after() in sequence.
    ///
    /// Every sandbox gets to inspect and mutate the output, even if an
    /// earlier sandbox already modified it.  This lets audit sandboxes
    /// see the final state.
    async fn after(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        for sandbox in &self.sandboxes {
            sandbox.after(tool_name, input, output).await?;
        }
        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::no_sandbox::DangerousNoSandbox;

    // -----------------------------------------------------------------------
    // Test sandboxes
    // -----------------------------------------------------------------------

    /// Always denies bash commands containing "rm".
    struct DenyRmSandbox;

    #[async_trait]
    impl Sandbox for DenyRmSandbox {
        async fn check(
            &self,
            tool_name: &str,
            input: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<SandboxDecision> {
            if tool_name == "bash" {
                let cmd = input["command"].as_str().unwrap_or("");
                if cmd.contains("rm ") {
                    return Ok(SandboxDecision::Deny {
                        reason: "rm commands are blocked".into(),
                    });
                }
            }
            Ok(SandboxDecision::Allow {
                input: input.clone(),
            })
        }
    }

    /// Rewrites bash commands by prepending "echo ".
    struct PrefixSandbox;

    #[async_trait]
    impl Sandbox for PrefixSandbox {
        async fn check(
            &self,
            tool_name: &str,
            input: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<SandboxDecision> {
            if tool_name == "bash" {
                let cmd = input["command"].as_str().unwrap_or("");
                return Ok(SandboxDecision::Allow {
                    input: serde_json::json!({"command": format!("echo {cmd}")}),
                });
            }
            Ok(SandboxDecision::Allow {
                input: input.clone(),
            })
        }
    }

    /// Appends " [audited]" to tool output.
    struct AuditAfterSandbox;

    #[async_trait]
    impl Sandbox for AuditAfterSandbox {
        async fn check(
            &self,
            _tool_name: &str,
            input: &serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<SandboxDecision> {
            Ok(SandboxDecision::Allow {
                input: input.clone(),
            })
        }

        async fn after(
            &self,
            _tool_name: &str,
            _input: &serde_json::Value,
            output: &mut ToolOutput,
        ) -> Result<()> {
            output.content.push_str(" [audited]");
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn empty_composite_allows_everything() {
        let sandbox = CompositeSandbox::new(vec![]);
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "rm -rf /"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn deny_short_circuits() {
        // DenyRm should block, PrefixSandbox should never run.
        let sandbox = CompositeSandbox::new(vec![
            Box::new(DenyRmSandbox),
            Box::new(PrefixSandbox),
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "rm -rf /"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Deny { reason } => {
                assert!(reason.contains("rm commands are blocked"));
            }
            other => panic!("expected Deny, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn allows_chain_rewrites() {
        // PrefixSandbox rewrites "ls" → "echo ls".
        // DangerousNoSandbox passes it through unchanged.
        let sandbox = CompositeSandbox::new(vec![
            Box::new(PrefixSandbox),
            Box::new(DangerousNoSandbox),
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "ls"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                assert_eq!(input["command"], "echo ls");
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_matching_tool_passes_through() {
        let sandbox = CompositeSandbox::new(vec![
            Box::new(DenyRmSandbox),
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"query": "test"});

        // DenyRmSandbox only cares about "bash" — other tools pass through.
        let decision = sandbox.check("web_search", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn after_runs_all_sandboxes() {
        let sandbox = CompositeSandbox::new(vec![
            Box::new(AuditAfterSandbox),
            Box::new(AuditAfterSandbox),
        ]);
        let input = serde_json::json!({});
        let mut output = ToolOutput::success("result");

        sandbox.after("bash", &input, &mut output).await.unwrap();
        // Both after() calls should have appended " [audited]".
        assert_eq!(output.content, "result [audited] [audited]");
    }

    #[tokio::test]
    async fn deny_then_allow_stops_at_deny() {
        let sandbox = CompositeSandbox::new(vec![
            Box::new(DenyRmSandbox),
            Box::new(DangerousNoSandbox),
        ]);
        let ctx = ToolContext::from_cwd().unwrap();

        // "rm" gets denied.
        let decision = sandbox
            .check("bash", &serde_json::json!({"command": "rm file"}), &ctx)
            .await
            .unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));

        // "ls" gets allowed.
        let decision = sandbox
            .check("bash", &serde_json::json!({"command": "ls"}), &ctx)
            .await
            .unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }
}
