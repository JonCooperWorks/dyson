// ===========================================================================
// Sandbox — the security gate between the LLM and tool execution.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `Sandbox` trait and `SandboxDecision` enum that gate every
//   tool call in the agent loop.  Before any tool runs, the sandbox gets
//   to inspect the call and decide: allow, deny, or redirect.  After the
//   tool runs, the sandbox can inspect and mutate the output.
//
// Module layout:
//   mod.rs            — Sandbox trait, SandboxDecision (this file)
//   no_sandbox.rs     — DangerousNoSandbox (passthrough, no restrictions)
//   os.rs             — OS command builders (Linux bwrap / macOS Apple Containers)
//   policy.rs         — SandboxPolicy types and PolicyTable
//   policy_sandbox.rs — PolicySandbox (the main sandbox implementation)
//
// Why a trait and not middleware?
//   The sandbox needs to make *semantic* decisions about tool calls.  It's
//   not just allow/deny — it can rewrite inputs, swap tools entirely, or
//   post-process outputs.  This is richer than HTTP middleware or a simple
//   permission check.  The trait approach lets you compose arbitrary
//   policies.
//
// How it fits in the agent loop:
//
//   LLM says: tool_use("bash", {"command": "rm -rf /"})
//     │
//     ▼
//   sandbox.check("bash", input, ctx)
//     │
//     ├── Allow { input }     → tool.run(&input, ctx) → sandbox.after(...)
//     ├── Deny { reason }     → ToolOutput::error(reason) back to LLM
//     └── Redirect { name, input } → different_tool.run(...) → sandbox.after(...)
//
// The Redirect variant enables transparent rerouting — the LLM says
// "read_file" thinking it's local, but the sandbox quietly sends it
// to S3.  The LLM doesn't know or care.
// ===========================================================================

pub mod no_sandbox;
pub mod os;
pub mod policy;
pub mod policy_sandbox;

use async_trait::async_trait;

use crate::error::Result;
use crate::tool::{ToolContext, ToolOutput};

/// Maximum tool output size (characters) before truncation.
///
/// Protects against MCP servers returning huge payloads, bash commands
/// producing excessive output, or any tool returning unexpectedly large
/// results that would blow up the context window.
pub(crate) const MAX_OUTPUT_CHARS: usize = 100_000;

// ---------------------------------------------------------------------------
// SandboxDecision
// ---------------------------------------------------------------------------

/// What the sandbox decided to do with a tool call.
///
/// Returned by [`Sandbox::check()`] before every tool execution.  The
/// agent loop matches on this to determine how to proceed.
#[derive(Debug)]
pub enum SandboxDecision {
    /// Allow the tool call with the given input.
    ///
    /// The input may be the original input unchanged, or a rewritten
    /// version (e.g., the sandbox added a `--read-only` flag to a
    /// command, or resolved a relative path to an absolute one).
    Allow { input: serde_json::Value },

    /// Deny the tool call entirely.
    ///
    /// `reason` is sent back to the LLM as an error `tool_result` so
    /// it can understand why the call was blocked and try something else.
    /// Be specific: "bash command 'rm -rf /' denied by sandbox policy"
    /// is better than "permission denied".
    Deny { reason: String },

    /// Redirect to a different tool with new input.
    ///
    /// The agent looks up `tool_name` in its tool registry and calls
    /// that tool instead.  The LLM doesn't know the redirect happened —
    /// it gets back a normal `tool_result` for its original `tool_use`.
    ///
    /// Use cases:
    /// - Route `read_file` to an S3-backed reader
    /// - Route `bash` to a container executor
    /// - Route `write_file` through a review/approval tool
    Redirect {
        tool_name: String,
        input: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// Sandbox trait
// ---------------------------------------------------------------------------

/// Policy layer that gates every tool execution in the agent.
///
/// Implementations can enforce security policies, audit tool usage,
/// redirect calls to alternative implementations, or rewrite inputs
/// and outputs.
///
/// ## Contract
///
/// - `check()` is called **before** every tool call.  It MUST return
///   a `SandboxDecision`.  Returning an `Err` is reserved for
///   infrastructure failures (sandbox couldn't evaluate the policy),
///   not for denying a call (use `Deny` for that).
///
/// - `after()` is called **after** a tool successfully executes.  It
///   receives a mutable reference to the output so it can redact secrets,
///   truncate, add audit metadata, etc.  The default impl is a no-op.
///
/// ## Thread safety
///
/// Sandboxes are `Send + Sync` because the agent may process tool calls
/// from multiple conversations (future: multi-session support).
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Inspect a tool call before execution.
    ///
    /// `tool_name` is the registered name of the tool (e.g., "bash").
    /// `input` is the JSON the LLM provided.  `ctx` is the execution
    /// context (working dir, env, cancellation token).
    ///
    /// Return `Allow` to proceed, `Deny` to block, or `Redirect` to
    /// send the call to a different tool.
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<SandboxDecision>;

    /// Post-process a tool's output after execution.
    ///
    /// Called only when the tool returned `Ok(ToolOutput)` (not on
    /// `Err(DysonError)`).  The mutable reference lets you modify the
    /// output in place — redact secrets, append audit info, truncate, etc.
    ///
    /// The default implementation does nothing.
    async fn after(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _output: &mut ToolOutput,
    ) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sandbox factory — build from config + CLI flags.
// ---------------------------------------------------------------------------

/// Build the sandbox from config, returning an `Arc` for shared ownership.
///
/// If `dangerous_no_sandbox` is true (from CLI flag), returns
/// `DangerousNoSandbox` regardless of config.  Otherwise returns a
/// `PolicySandbox` that enforces per-tool capability policies.
///
/// The `Arc` wrapper enables subagents to share the parent's sandbox without
/// cloning the entire sandbox tree.
pub fn create_sandbox(
    config: &crate::config::SandboxConfig,
    dangerous_no_sandbox: bool,
) -> std::sync::Arc<dyn Sandbox> {
    if dangerous_no_sandbox {
        tracing::warn!("all sandboxes disabled via --dangerous-no-sandbox");
        return std::sync::Arc::new(no_sandbox::DangerousNoSandbox);
    }

    if config.disabled.iter().any(|s| s == "os") {
        tracing::warn!(
            "ignoring sandbox.disabled config — sandbox can only be disabled \
             via the --dangerous-no-sandbox CLI flag"
        );
    }

    // Verify the OS sandbox binary is available.  Without it, bash commands
    // would run unsandboxed — too dangerous for an LLM-controlled agent.
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let has_container = Command::new("which")
            .arg("container")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_container {
            tracing::error!(
                "macOS sandbox requires the 'container' CLI (Apple Containers). \
                 Install it with: brew install container (or from github.com/apple/container). \
                 Refusing to start without OS sandbox — use --dangerous-no-sandbox to override."
            );
            // Return DangerousNoSandbox but mark it so callers know it's a failure.
            // Actually, we should panic/exit since this is a security-critical failure.
            std::process::exit(1);
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        let has_bwrap = Command::new("which")
            .arg("bwrap")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_bwrap {
            tracing::error!(
                "Linux sandbox requires bubblewrap (bwrap). \
                 Install it with: apt install bubblewrap (or: dnf install bubblewrap). \
                 Refusing to start without OS sandbox — use --dangerous-no-sandbox to override."
            );
            std::process::exit(1);
        }
    }

    let working_dir =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));

    tracing::info!(
        tool_policies = config.tool_policies.len(),
        "policy sandbox enabled"
    );

    std::sync::Arc::new(policy_sandbox::PolicySandbox::new(
        &config.tool_policies,
        &working_dir,
    ))
}
