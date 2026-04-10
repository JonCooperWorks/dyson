// ===========================================================================
// DangerousNoSandbox — the "I know what I'm doing" passthrough sandbox.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `Sandbox` trait as a complete no-op: every tool call is
//   allowed, no inputs are rewritten, no outputs are modified.  This is
//   the default sandbox for Phase 1 and for users who explicitly opt out
//   of sandboxing with `--dangerous-no-sandbox`.
//
// Why "Dangerous" in the name?
//   Running an LLM agent with unrestricted tool access is inherently risky.
//   The LLM can run arbitrary bash commands, read/write any file, and make
//   network requests.  The name is intentionally alarming to remind users
//   (and developers) that this is the "no seatbelts" mode.  In production,
//   you'd swap this for a ContainerSandbox or policy-based sandbox.
//
// When to use:
//   - Local development where you trust the LLM's tool calls
//   - Testing and CI where the environment is already sandboxed
//   - Phase 1 of Dyson (no other sandbox impl exists yet)
// ===========================================================================

use async_trait::async_trait;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::ToolContext;

// ---------------------------------------------------------------------------
// DangerousNoSandbox
// ---------------------------------------------------------------------------

/// Passthrough sandbox that allows every tool call without restriction.
///
/// Selected via `--dangerous-no-sandbox` CLI flag.  Does not modify
/// inputs or outputs.  Logs every tool call for observability.
///
/// ## Why this exists instead of just `Option<Box<dyn Sandbox>>`
///
/// Making the sandbox mandatory (not optional) means the agent loop always
/// has the same code path: `sandbox.check() → tool.run() → sandbox.after()`.
/// No `if let Some(sandbox) = ...` branching.  When you add a real sandbox,
/// you just swap the impl — zero changes to the agent loop.
pub struct DangerousNoSandbox;

#[async_trait]
impl Sandbox for DangerousNoSandbox {
    /// Always allows the call with the original input, unchanged.
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        tracing::info!(tool = tool_name, input = %input, "tool call allowed (no sandbox)");
        Ok(SandboxDecision::Allow {
            input: input.clone(),
        })
    }

    fn skip_path_validation(&self) -> bool {
        true
    }

    // `after()` uses the default no-op from the trait.
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolContext, ToolOutput};

    #[tokio::test]
    async fn always_allows() {
        let sandbox = DangerousNoSandbox;
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "rm -rf /"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input: allowed } => {
                assert_eq!(allowed, serde_json::json!({"command": "rm -rf /"}));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn after_is_noop() {
        let sandbox = DangerousNoSandbox;
        let input = serde_json::json!({});
        let mut output = ToolOutput::success("original content");

        sandbox.after("bash", &input, &mut output).await.unwrap();
        assert_eq!(output.content, "original content");
        assert!(!output.is_error);
    }
}
