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

use crate::sandbox::{Sandbox, SandboxBypassGuard, SandboxDecision};
use crate::tool::ToolContext;
use dyson_core::error::Result;

// ---------------------------------------------------------------------------
// DangerousNoSandbox
// ---------------------------------------------------------------------------

/// Passthrough sandbox that allows every tool call without restriction.
///
/// Selected via `--dangerous-no-sandbox` CLI flag.  Does not modify
/// inputs or outputs.  Logs every tool call for observability.
///
/// The held `SandboxBypassGuard` is what tools call out for when
/// they need to step outside the working directory — this is the
/// only place in the codebase that hands one out to consumers.
///
/// ## Why this exists instead of just `Option<Box<dyn Sandbox>>`
///
/// Making the sandbox mandatory (not optional) means the agent loop always
/// has the same code path: `sandbox.check() → tool.run() → sandbox.after()`.
/// No `if let Some(sandbox) = ...` branching.  When you add a real sandbox,
/// you just swap the impl — zero changes to the agent loop.
pub struct DangerousNoSandbox {
    bypass: SandboxBypassGuard,
}

impl DangerousNoSandbox {
    pub fn new(bypass: SandboxBypassGuard) -> Self {
        Self { bypass }
    }
}

#[async_trait]
impl Sandbox for DangerousNoSandbox {
    /// Always allows the call with the original input, unchanged.
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        let redacted = crate::sandbox::policy_sandbox::redact_secrets(&input.to_string());
        tracing::info!(
            tool = tool_name,
            input = %redacted,
            "tool call allowed (no sandbox)"
        );
        Ok(SandboxDecision::Allow {
            input: input.clone(),
        })
    }

    fn sandbox_bypass(&self) -> Option<&SandboxBypassGuard> {
        Some(&self.bypass)
    }

    /// Runs the script unwrapped, exactly as a `bash` call would run here.
    async fn check_skill_script(
        &self,
        command: &str,
        _skill_dir: &std::path::Path,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        tracing::info!(command, "script skill allowed (no sandbox)");
        Ok(SandboxDecision::Allow {
            input: serde_json::json!({ "command": command }),
        })
    }
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
        let sandbox = DangerousNoSandbox::new(SandboxBypassGuard::for_test());
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
    async fn skill_script_runs_unwrapped() {
        // Same posture as bash under --dangerous-no-sandbox.
        let sandbox = DangerousNoSandbox::new(SandboxBypassGuard::for_test());
        let ctx = ToolContext::from_cwd().unwrap();
        let decision = sandbox
            .check_skill_script("exec bash run.sh", std::path::Path::new("/skills/x"), &ctx)
            .await
            .unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                assert_eq!(input, serde_json::json!({"command": "exec bash run.sh"}));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sandboxes_without_script_support_deny_scripts() {
        struct Bare;
        #[async_trait]
        impl Sandbox for Bare {
            async fn check(
                &self,
                _: &str,
                input: &serde_json::Value,
                _: &ToolContext,
            ) -> Result<SandboxDecision> {
                Ok(SandboxDecision::Allow {
                    input: input.clone(),
                })
            }
        }
        let decision = Bare
            .check_skill_script(
                "exec bash run.sh",
                std::path::Path::new("/skills/x"),
                &ToolContext::from_cwd().unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[test]
    fn redact_secrets_strips_bearer_from_log_payload() {
        // Sanity check that the secret-redaction helper used when logging
        // tool-call input in DangerousNoSandbox actually redacts common
        // secret shapes.  If this drifts, secrets will hit INFO logs.
        let raw = serde_json::json!({
            "command": "curl -H 'Authorization: Bearer sk-live-topsecret' https://api",
            "env": "API_KEY=sk-live-topsecret"
        })
        .to_string();
        let redacted = crate::sandbox::policy_sandbox::redact_secrets(&raw);
        assert!(
            !redacted.contains("sk-live-topsecret"),
            "expected bearer + API_KEY to be redacted, got: {redacted}"
        );
    }

    #[tokio::test]
    async fn after_is_noop() {
        let sandbox = DangerousNoSandbox::new(SandboxBypassGuard::for_test());
        let input = serde_json::json!({});
        let mut output = ToolOutput::success("original content");

        sandbox.after("bash", &input, &mut output).await.unwrap();
        assert_eq!(output.content, "original content");
        assert!(!output.is_error);
    }
}
