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
//   mod.rs         — Sandbox trait, SandboxDecision (this file)
//   no_sandbox.rs  — DangerousNoSandbox (passthrough, no restrictions)
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
//     ├── Allow { input }     → tool.run(input, ctx) → sandbox.after(...)
//     ├── Deny { reason }     → ToolOutput::error(reason) back to LLM
//     └── Redirect { name, input } → different_tool.run(...) → sandbox.after(...)
//
// Future sandbox implementations:
//
//   ContainerSandbox   — routes bash/python through a Docker container,
//                         rewrites file paths to container mounts
//   BlacklistSandbox   — denies specific tools or command patterns
//   S3Sandbox          — redirects file read/write to S3 paths instead
//                         of the host filesystem
//   AuditSandbox       — allows everything but logs all calls to a file
//   CompositeSandbox   — chains multiple sandboxes; first Deny wins,
//                         Redirects compose, Allow is the default
//
// The Redirect variant is the key innovation.  It doesn't just block
// things — it can transparently reroute tool calls to different
// implementations.  The LLM says "read_file" thinking it's local, but
// the sandbox quietly sends it to S3.  The LLM doesn't know or care.
// ===========================================================================

pub mod composite;
pub mod docker;
pub mod no_sandbox;
pub mod os;

use async_trait::async_trait;

use crate::error::Result;
use crate::tool::{ToolContext, ToolOutput};

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

/// Build the sandbox from config.
///
/// If `dangerous_no_sandbox` is true (from CLI flag), returns
/// `DangerousNoSandbox` regardless of config.  This is the only way to
/// disable all sandboxes — it cannot be done from config.
///
/// Otherwise, builds a `CompositeSandbox` with all non-disabled sandboxes.
/// If the composite has no sandboxes (all disabled via config), it still
/// functions — it just allows everything (like an empty pipeline).
pub fn create_sandbox(
    config: &crate::config::SandboxConfig,
    dangerous_no_sandbox: bool,
) -> Box<dyn Sandbox> {
    if dangerous_no_sandbox {
        tracing::warn!("all sandboxes disabled via --dangerous-no-sandbox");
        return Box::new(no_sandbox::DangerousNoSandbox);
    }

    let disabled = &config.disabled;
    let mut sandboxes: Vec<Box<dyn Sandbox>> = Vec::new();

    // OS sandbox (default — always on unless explicitly disabled).
    //
    // Uses the operating system's native sandboxing:
    // - macOS: sandbox-exec (Seatbelt) — denies network, restricts writes
    // - Linux: falls back to unsandboxed (with warning) until bwrap support
    if !disabled.iter().any(|s| s == "os") {
        let working_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/tmp".to_string());

        let profile = config
            .os_profile
            .as_deref()
            .unwrap_or("default");

        tracing::info!(profile = profile, "OS sandbox enabled");
        sandboxes.push(Box::new(os::OsSandbox::named_profile(
            profile,
            &working_dir,
        )));
    } else {
        tracing::info!("OS sandbox disabled via config");
    }

    // Docker sandbox (optional — only if configured).
    if !disabled.iter().any(|s| s == "docker") {
        if let Some(ref docker_config) = config.docker {
            tracing::info!(
                container = docker_config.container,
                "docker sandbox enabled"
            );
            match docker::DockerSandbox::new(&docker_config.container) {
                Ok(sandbox) => sandboxes.push(Box::new(sandbox)),
                Err(e) => {
                    tracing::error!(error = %e, "invalid docker sandbox config — skipping");
                }
            }
        }
    } else {
        tracing::info!("docker sandbox disabled via config");
    }

    // Future sandboxes go here:
    // if !disabled.contains("file") { ... }
    // if !disabled.contains("network") { ... }
    // if !disabled.contains("audit") { ... }

    tracing::info!(count = sandboxes.len(), "sandbox pipeline built");

    Box::new(composite::CompositeSandbox::new(sandboxes))
}
