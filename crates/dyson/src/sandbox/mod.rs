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

    /// Returns the active [`SandboxBypassGuard`] iff this sandbox skips
    /// working-directory path validation.  Only `DangerousNoSandbox`
    /// returns `Some`; `PolicySandbox` returns `None`.
    ///
    /// Tools that need to step outside the working directory
    /// (e.g. `send_file`) take the returned guard as proof of the
    /// bypass — there is no other way to mint one in production
    /// code outside `main.rs`'s CLI argument boundary.
    fn sandbox_bypass(&self) -> Option<&SandboxBypassGuard> {
        None
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackendStatus {
    Ready,
    DangerousDisabled,
    MissingBackend(&'static str),
    UnsupportedPlatform,
}

/// Unforgeable capability proving the caller has explicitly opted out
/// of the OS sandbox.  The bypass used to be a `bool` that was
/// plumbed through ~90 sites; an extra `bool` parameter slipping in
/// undetected could silently disable sandboxing for a tool.  The
/// typed guard makes the bypass impossible to fabricate accidentally:
/// the only private field is `_seal: ()`, and the only constructors
/// are the explicit, named ones below.
///
/// Pattern mirrors `dyson-swarm`'s `RawKmsAccessGuard` /
/// `SystemBypassGuard` / `LiveSystemCipher`.
///
/// CLAUDE.md says it best: "The sandbox is the security boundary.
/// Never add a bypass.  --dangerous-no-sandbox is an explicit
/// opt-in, not a fallback."  This type enforces that with the
/// compiler.
#[must_use = "SandboxBypassGuard does nothing on its own; thread it into ToolContext or pass to path validation"]
#[derive(Clone, Debug)]
pub struct SandboxBypassGuard {
    purpose: SandboxBypassPurpose,
    _seal: (),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxBypassPurpose {
    /// Operator passed `--dangerous-no-sandbox` on the CLI.  This is
    /// the only production-allowed mint.
    CliExplicitOptIn,
    /// In-process derivation for a subagent that inherits the
    /// parent's bypass posture without re-parsing CLI args.
    InheritedFromParent,
    /// Test-only — gated behind `cfg(test)` and the
    /// `sandbox-bypass-test` feature for downstream test crates.
    #[cfg(any(test, feature = "sandbox-bypass-test"))]
    Test,
}

impl SandboxBypassPurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CliExplicitOptIn => "cli_explicit_opt_in",
            Self::InheritedFromParent => "inherited_from_parent",
            #[cfg(any(test, feature = "sandbox-bypass-test"))]
            Self::Test => "test",
        }
    }
}

impl SandboxBypassGuard {
    /// Mint a guard because the operator passed `--dangerous-no-sandbox`
    /// on the CLI.  The only production-allowed constructor.
    pub fn for_cli_explicit_opt_in() -> Self {
        Self::mint(SandboxBypassPurpose::CliExplicitOptIn)
    }

    /// Mint a guard for a subagent that inherits its parent's
    /// already-validated bypass posture.  Crate-private so this
    /// cannot be reached from out-of-crate code.
    pub(crate) fn inherited_from_parent() -> Self {
        Self::mint(SandboxBypassPurpose::InheritedFromParent)
    }

    /// Test-only constructor.
    #[cfg(any(test, feature = "sandbox-bypass-test"))]
    pub fn for_test() -> Self {
        Self::mint(SandboxBypassPurpose::Test)
    }

    fn mint(purpose: SandboxBypassPurpose) -> Self {
        tracing::debug!(
            target: "sandbox.bypass",
            purpose = purpose.as_str(),
            "SandboxBypassGuard minted: tool calls will skip path validation"
        );
        Self { purpose, _seal: () }
    }

    pub fn purpose(&self) -> SandboxBypassPurpose {
        self.purpose
    }
}

/// Mint a guard from the CLI flag's boolean value.  This is the
/// type-system boundary between argument parsing (which has to
/// accept `bool` because that's the wire format) and the rest of
/// the codebase (which threads `Option<SandboxBypassGuard>` so the
/// bypass carries its provenance).
pub fn sandbox_bypass_from_cli_flag(flag: bool) -> Option<SandboxBypassGuard> {
    if flag {
        Some(SandboxBypassGuard::for_cli_explicit_opt_in())
    } else {
        None
    }
}

pub fn sandbox_backend_status_for_target(
    target_os: &str,
    sandbox_bypass: Option<&SandboxBypassGuard>,
    has_bwrap: bool,
    has_container: bool,
) -> SandboxBackendStatus {
    if sandbox_bypass.is_some() {
        return SandboxBackendStatus::DangerousDisabled;
    }
    match target_os {
        "linux" if has_bwrap => SandboxBackendStatus::Ready,
        "linux" => SandboxBackendStatus::MissingBackend("bwrap"),
        "macos" if has_container => SandboxBackendStatus::Ready,
        "macos" => SandboxBackendStatus::MissingBackend("container"),
        _ => SandboxBackendStatus::UnsupportedPlatform,
    }
}

pub fn current_sandbox_backend_status(
    sandbox_bypass: Option<&SandboxBypassGuard>,
) -> SandboxBackendStatus {
    sandbox_backend_status_for_target(
        std::env::consts::OS,
        sandbox_bypass,
        binary_on_path("bwrap"),
        binary_on_path("container"),
    )
}

fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            candidate
                .metadata()
                .map(|meta| meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
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
    sandbox_bypass: Option<SandboxBypassGuard>,
) -> std::sync::Arc<dyn Sandbox> {
    if config.disabled.iter().any(|s| s == "os") {
        tracing::warn!(
            "ignoring sandbox.disabled config — sandbox can only be disabled \
             via the --dangerous-no-sandbox CLI flag"
        );
    }

    if config.os_profile.is_some() {
        tracing::warn!(
            "ignoring sandbox.os_profile config — OS sandbox profiles are not \
             yet implemented; tool_policies control sandbox behaviour"
        );
    }

    match current_sandbox_backend_status(sandbox_bypass.as_ref()) {
        SandboxBackendStatus::DangerousDisabled => {
            tracing::warn!("all sandboxes disabled via --dangerous-no-sandbox");
            // Unwrap: we just matched DangerousDisabled, which only
            // fires when sandbox_bypass.is_some().
            let guard = sandbox_bypass.expect(
                "DangerousDisabled implies sandbox_bypass.is_some() — checked by status fn",
            );
            return std::sync::Arc::new(no_sandbox::DangerousNoSandbox::new(guard));
        }
        SandboxBackendStatus::Ready => {}
        SandboxBackendStatus::MissingBackend("bwrap") => {
            tracing::error!(
                "Linux sandbox requires bubblewrap (bwrap). \
                 Install it with: apt install bubblewrap (or: dnf install bubblewrap). \
                 Refusing to start without OS sandbox — use --dangerous-no-sandbox to override."
            );
            std::process::exit(1);
        }
        SandboxBackendStatus::MissingBackend("container") => {
            tracing::error!(
                "macOS sandbox requires the 'container' CLI (Apple Containers). \
                 Install it with: brew install container (or from github.com/apple/container). \
                 Refusing to start without OS sandbox — use --dangerous-no-sandbox to override."
            );
            std::process::exit(1);
        }
        SandboxBackendStatus::MissingBackend(binary) => {
            tracing::error!(
                binary,
                "OS sandbox backend is missing; refusing to start without --dangerous-no-sandbox"
            );
            std::process::exit(1);
        }
        SandboxBackendStatus::UnsupportedPlatform => {
            tracing::error!(
                os = std::env::consts::OS,
                "OS sandbox is unsupported on this platform; refusing to start without --dangerous-no-sandbox"
            );
            std::process::exit(1);
        }
    }

    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));

    tracing::info!(
        tool_policies = config.tool_policies.len(),
        "policy sandbox enabled"
    );

    std::sync::Arc::new(policy_sandbox::PolicySandbox::new(
        &config.tool_policies,
        &working_dir,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_platform_requires_dangerous_no_sandbox() {
        assert_eq!(
            sandbox_backend_status_for_target("freebsd", None, false, false),
            SandboxBackendStatus::UnsupportedPlatform,
            "D6 unsupported targets must refuse to start unless --dangerous-no-sandbox is set"
        );
        let bypass = SandboxBypassGuard::for_test();
        assert_eq!(
            sandbox_backend_status_for_target("freebsd", Some(&bypass), false, false),
            SandboxBackendStatus::DangerousDisabled,
            "D6 explicit dangerous opt-out is the only unsupported-target escape hatch"
        );
    }

    #[test]
    fn cli_flag_false_yields_none() {
        assert!(sandbox_bypass_from_cli_flag(false).is_none());
    }

    #[test]
    fn cli_flag_true_yields_some_with_correct_purpose() {
        let guard = sandbox_bypass_from_cli_flag(true).unwrap();
        assert_eq!(guard.purpose(), SandboxBypassPurpose::CliExplicitOptIn);
    }

    #[test]
    fn guard_purposes_round_trip_as_str() {
        assert_eq!(
            SandboxBypassPurpose::CliExplicitOptIn.as_str(),
            "cli_explicit_opt_in"
        );
        assert_eq!(
            SandboxBypassPurpose::InheritedFromParent.as_str(),
            "inherited_from_parent"
        );
        assert_eq!(SandboxBypassPurpose::Test.as_str(), "test");
    }
}
