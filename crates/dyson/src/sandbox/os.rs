// ===========================================================================
// OS-level command builders — translate SandboxPolicy to platform-specific
// sandbox wrappers for bash commands.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides pure functions that wrap a bash command string in OS-native
//   sandboxing.  These are called by PolicySandbox when it needs to enforce
//   a policy on a bash tool call.
//
// Platforms:
//
//   Linux (bubblewrap / bwrap):
//     Creates Linux namespaces for filesystem and PID isolation.
//     No root required.  Used by Flatpak in production.
//
//     Install: apt install bubblewrap  (or: dnf install bubblewrap)
//
//     Example:
//       bwrap --ro-bind / / --dev /dev --proc /proc \
//             --tmpfs /tmp --bind <cwd> <cwd> \
//             --die-with-parent \
//             bash -c '<command>'
//
//   macOS (Apple Containers):
//     Lightweight Linux VMs via Apple's Virtualization framework.
//     Apple Silicon only.  Requires `container` CLI from apple/container.
//
//     Install: brew install container  (or: from github.com/apple/container)
//
//     Example:
//       container run --rm --network none \
//         -v /workspace:/workspace \
//         -w /workspace \
//         alpine:latest sh -c '<command>'
//
//     Note: Commands run in a Linux environment inside the container,
//     not natively on macOS.  Most shell commands are portable, but
//     macOS-specific tools (brew, open, etc.) won't be available.
//
// Both builders are NOT gated by #[cfg] so they can be tested on any
// platform.  Only the *caller* (PolicySandbox) uses #[cfg] to select
// which builder to invoke at runtime.
// ===========================================================================

use crate::sandbox::policy::{Access, PathAccess, SandboxPolicy};
use crate::util::escape_single_quotes;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Linux: bubblewrap (bwrap)
// ---------------------------------------------------------------------------

/// Essential system directories needed for bash to function.
///
/// Always mounted read-only when file_read is restricted, so that shell
/// builtins, coreutils, and shared libraries are available.
const ESSENTIAL_SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum MountSpec {
    Root { writable: bool },
    ReadOnly(PathBuf),
    Writable(PathBuf),
    Tmpfs(PathBuf),
}

fn policy_to_mount_specs(policy: &SandboxPolicy) -> Vec<MountSpec> {
    let mut specs = Vec::new();

    match &policy.file_read {
        PathAccess::Allow if matches!(policy.file_write, PathAccess::Allow) => {
            specs.push(MountSpec::Root { writable: true });
        }
        PathAccess::Allow => {
            specs.push(MountSpec::Root { writable: false });
            append_writable_mount_specs(&mut specs, &policy.file_write);
        }
        PathAccess::RestrictTo(read_paths) => {
            for path in read_paths {
                if !essential_system_dir_covers(path) {
                    specs.push(MountSpec::ReadOnly(path.clone()));
                }
            }
            append_writable_mount_specs(&mut specs, &policy.file_write);
        }
        PathAccess::Deny => {
            append_writable_mount_specs(&mut specs, &policy.file_write);
        }
    }

    specs
}

fn append_writable_mount_specs(specs: &mut Vec<MountSpec>, file_write: &PathAccess) {
    if let PathAccess::RestrictTo(paths) = file_write {
        for path in paths {
            if is_tmp_path(path) {
                specs.push(MountSpec::Tmpfs(PathBuf::from("/tmp")));
            } else {
                specs.push(MountSpec::Writable(path.clone()));
            }
        }
    }
}

fn essential_system_dir_covers(path: &Path) -> bool {
    ESSENTIAL_SYSTEM_DIRS
        .iter()
        .any(|sys| path.starts_with(sys))
}

fn is_tmp_path(path: &Path) -> bool {
    path == Path::new("/tmp") || path == Path::new("/private/tmp")
}

/// Build a Linux bwrap command from a `SandboxPolicy`.
///
/// Translates intent into bwrap flags:
/// - `file_read: Allow` + `file_write: Allow` → `--bind / /`
/// - `file_read: Allow` + `file_write: Deny/RestrictTo` → `--ro-bind / /` + writable binds
/// - `file_read: RestrictTo/Deny` → selective read-only binds for allowed paths + system dirs
/// - `network: Deny` → `--unshare-net` (full network namespace isolation)
/// - `network: Allow` → network namespace shared with host (for pip, API calls)
/// - `process_exec: Deny` → `--unshare-pid` (PID visibility only; does NOT prevent exec)
///
/// When `/tmp` appears in writable paths, `--tmpfs /tmp` is used instead of
/// `--bind /tmp /tmp` to provide an isolated temporary directory.
pub fn build_bwrap_command_from_policy(
    command: &str,
    policy: &SandboxPolicy,
    _working_dir: &str,
) -> String {
    let escaped = escape_single_quotes(command);
    let mut parts = Vec::new();

    parts.push("bwrap".to_string());

    let mount_specs = policy_to_mount_specs(policy);
    if !matches!(policy.file_read, PathAccess::Allow) {
        // Restricted or denied reads — no root bind.
        // Mount essential system directories read-only so bash works.
        for dir in ESSENTIAL_SYSTEM_DIRS {
            parts.push(format!("--ro-bind {dir} {dir}"));
        }
    }
    format_bwrap_mounts(&mount_specs, &mut parts);

    fn format_bwrap_mounts(specs: &[MountSpec], parts: &mut Vec<String>) {
        for spec in specs {
            match spec {
                MountSpec::Root { writable: true } => parts.push("--bind / /".to_string()),
                MountSpec::Root { writable: false } => parts.push("--ro-bind / /".to_string()),
                MountSpec::ReadOnly(path) => {
                    let p = escape_single_quotes(&path.to_string_lossy());
                    parts.push(format!("--ro-bind '{p}' '{p}'"));
                }
                MountSpec::Writable(path) => {
                    let p = escape_single_quotes(&path.to_string_lossy());
                    parts.push(format!("--bind '{p}' '{p}'"));
                }
                MountSpec::Tmpfs(path) => {
                    let p = escape_single_quotes(&path.to_string_lossy());
                    parts.push(format!("--tmpfs {p}"));
                }
            }
        }
    }

    // Always need /dev and /proc.
    parts.push("--dev /dev".to_string());
    parts.push("--proc /proc".to_string());

    // Network: isolate into a fresh namespace when policy denies network.
    // When allowed, the host network namespace is shared so skills can
    // reach pip, package indexes, and the Anthropic API.
    if policy.network == Access::Deny {
        parts.push("--unshare-net".to_string());
    }

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

/// Build a bwrap argv (not a shell string) for wrapping an MCP stdio
/// subprocess.  Unlike `build_bwrap_command_from_policy`, this returns
/// a Vec<String> suitable for direct `Command::new("bwrap").args(...)`
/// use — no shell interpretation, no escaping pitfalls.
///
/// The default policy is conservative but pragmatic for MCP:
/// - read-only root (so servers can read /usr, /etc, /lib)
/// - tmpfs `/tmp` (isolated ephemeral writes)
/// - PID namespace (`--unshare-pid`) to hide host processes
/// - `--die-with-parent` so the child can't outlive Dyson
/// - network: shared by default (MCP servers typically need APIs);
///   set `deny_network` to isolate
///
/// `command` and `args` are appended last so the child sees them as
/// argv[0..] — exactly as if invoked directly.
pub fn build_bwrap_argv_for_mcp_stdio(
    command: &str,
    args: &[String],
    deny_network: bool,
) -> Vec<String> {
    let mut argv = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
        "--unshare-pid".to_string(),
        "--die-with-parent".to_string(),
    ];
    if deny_network {
        argv.push("--unshare-net".to_string());
    }
    argv.push("--".to_string());
    argv.push(command.to_string());
    for a in args {
        argv.push(a.clone());
    }
    argv
}

// ---------------------------------------------------------------------------
// macOS: Apple Containers
// ---------------------------------------------------------------------------

/// Build a macOS Apple Container command from a `SandboxPolicy`.
///
/// Uses `container run` (from apple/container) to execute the command
/// in a lightweight Linux VM with controlled mounts and networking.
///
/// Translates intent into container flags:
/// - `network: Deny` → `--network none`
/// - `file_write: RestrictTo(paths)` → `-v path:path` (writable bind mounts)
/// - `file_read: RestrictTo(paths)` → `-v path:path:ro` (read-only bind mounts)
/// - `/tmp` in writable paths → `--tmpfs /tmp` (isolated ephemeral storage)
///
/// Paths already mounted writable are not duplicated as read-only mounts.
pub fn build_container_command_from_policy(
    command: &str,
    policy: &SandboxPolicy,
    working_dir: &str,
) -> String {
    let escaped = escape_single_quotes(command);
    let wd = escape_single_quotes(working_dir);
    let mut parts = Vec::new();

    parts.push("container run --rm".to_string());

    // Network isolation.
    if policy.network == Access::Deny {
        parts.push("--network none".to_string());
    }

    let mount_specs = policy_to_mount_specs(policy);

    // Track writable paths to avoid duplicate read-only mounts.
    let mut mounted_rw: Vec<String> = Vec::new();

    // Container CLI wants writable mounts before read-only mounts.
    format_container_writable_mounts(&mount_specs, &mut parts, &mut mounted_rw, &wd, working_dir);
    format_container_read_only_mounts(&mount_specs, &mut parts, &mounted_rw, &wd, working_dir);

    fn format_container_writable_mounts(
        specs: &[MountSpec],
        parts: &mut Vec<String>,
        mounted_rw: &mut Vec<String>,
        wd: &str,
        working_dir: &str,
    ) {
        for spec in specs {
            match spec {
                MountSpec::Root { writable: true } => {
                    parts.push(format!("-v '{wd}':'{wd}'"));
                    mounted_rw.push(working_dir.to_string());
                }
                MountSpec::Writable(path) => {
                    let p = path.to_string_lossy();
                    let pe = escape_single_quotes(&p);
                    parts.push(format!("-v '{pe}':'{pe}'"));
                    mounted_rw.push(p.to_string());
                }
                MountSpec::Tmpfs(path) => {
                    let p = escape_single_quotes(&path.to_string_lossy());
                    parts.push(format!("--tmpfs {p}"));
                }
                MountSpec::Root { writable: false } | MountSpec::ReadOnly(_) => {}
            }
        }
    }

    fn format_container_read_only_mounts(
        specs: &[MountSpec],
        parts: &mut Vec<String>,
        mounted_rw: &[String],
        wd: &str,
        working_dir: &str,
    ) {
        for spec in specs {
            match spec {
                MountSpec::Root { writable: false } => {
                    if !mounted_rw.iter().any(|p| p == working_dir) {
                        parts.push(format!("-v '{wd}':'{wd}':ro"));
                    }
                }
                MountSpec::ReadOnly(path) => {
                    let p = path.to_string_lossy();
                    let already_writable = mounted_rw.iter().any(|rw| rw == p.as_ref());
                    if !already_writable {
                        let pe = escape_single_quotes(&p);
                        parts.push(format!("-v '{pe}':'{pe}':ro"));
                    }
                }
                MountSpec::Root { writable: true }
                | MountSpec::Writable(_)
                | MountSpec::Tmpfs(_) => {}
            }
        }
    }

    // Working directory inside the container.
    parts.push(format!("-w '{wd}'"));

    // Image and command.
    parts.push(format!("alpine:latest sh -c '{escaped}'"));

    parts.join(" ")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        assert!(
            cmd.contains("--unshare-net"),
            "network Deny must unshare net"
        );
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
    fn bwrap_policy_deny_network_unshares_net() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        assert!(
            cmd.contains("--unshare-net"),
            "network Deny must produce --unshare-net, got: {cmd}"
        );
    }

    #[test]
    fn bwrap_policy_allow_network_omits_unshare() {
        let policy = SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_bwrap_command_from_policy("ls", &policy, "/workspace");
        assert!(
            !cmd.contains("--unshare-net"),
            "network Allow must not produce --unshare-net, got: {cmd}"
        );
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
    // Apple Container command builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn container_deny_network() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("container run --rm"));
        assert!(cmd.contains("--network none"));
        assert!(cmd.contains("-v '/workspace':'/workspace'"));
        assert!(cmd.contains("-w '/workspace'"));
        assert!(cmd.contains("alpine:latest sh -c 'ls'"));
    }

    #[test]
    fn container_allow_network() {
        let policy = SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Allow,
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("curl example.com", &policy, "/workspace");
        assert!(!cmd.contains("--network none"));
        assert!(cmd.contains("-v '/workspace':'/workspace'"));
    }

    #[test]
    fn container_deny_all_writes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("ls", &policy, "/workspace");
        // Should not have any writable mounts.
        assert!(!cmd.contains("-v '/workspace':'/workspace'\n"));
        // Should have read-only mount for working dir.
        assert!(cmd.contains("-v '/workspace':'/workspace':ro"));
    }

    #[test]
    fn container_tmp_uses_tmpfs() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::RestrictTo(vec![
                PathBuf::from("/workspace"),
                PathBuf::from("/tmp"),
            ]),
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("ls", &policy, "/workspace");
        assert!(cmd.contains("--tmpfs /tmp"), "should use tmpfs for /tmp");
        assert!(cmd.contains("-v '/workspace':'/workspace'"));
    }

    #[test]
    fn container_read_only_mounts_skip_writable() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("ls", &policy, "/workspace");
        // /workspace should be writable, not read-only.
        assert!(cmd.contains("-v '/workspace':'/workspace'"));
        assert!(
            !cmd.contains(":ro"),
            "should not have ro mount for writable path"
        );
    }

    #[test]
    fn container_separate_read_and_write_paths() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![
                PathBuf::from("/workspace"),
                PathBuf::from("/data"),
            ]),
            file_write: PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]),
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("ls", &policy, "/workspace");
        // /workspace writable.
        assert!(cmd.contains("-v '/workspace':'/workspace'"));
        // /data read-only.
        assert!(cmd.contains("-v '/data':'/data':ro"));
    }

    #[test]
    fn container_escapes_command_quotes() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Allow,
            file_write: PathAccess::Deny,
            process_exec: Access::Allow,
        };
        let cmd = build_container_command_from_policy("echo 'hello'", &policy, "/workspace");
        assert!(cmd.contains("'\\''"));
    }

    #[test]
    fn container_deny_reads_no_mounts() {
        let policy = SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        };
        let cmd = build_container_command_from_policy("echo ok", &policy, "/workspace");
        assert!(cmd.contains("container run --rm"));
        assert!(cmd.contains("--network none"));
        assert!(!cmd.contains("-v "), "should have no volume mounts");
    }

    // -----------------------------------------------------------------------
    // Platform-specific execution tests
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
        let ctx = crate::tool::ToolContext::from_cwd().unwrap();
        let output = tool
            .run(&serde_json::json!({"command": cmd}), &ctx)
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
        let ctx = crate::tool::ToolContext::from_cwd().unwrap();
        let output = tool
            .run(&serde_json::json!({"command": cmd}), &ctx)
            .await
            .unwrap();
        assert!(
            output.is_error,
            "expected write to /var/tmp to be blocked: {}",
            output.content
        );
    }

    #[test]
    fn mcp_stdio_argv_default_shares_network() {
        let argv = build_bwrap_argv_for_mcp_stdio(
            "npx",
            &["-y".to_string(), "@ctx/server".to_string()],
            false,
        );
        assert!(argv.contains(&"--ro-bind".to_string()));
        assert!(argv.contains(&"--die-with-parent".to_string()));
        assert!(argv.contains(&"--unshare-pid".to_string()));
        assert!(
            !argv.contains(&"--unshare-net".to_string()),
            "default must share network so MCP servers can reach APIs"
        );
        // Command + args appear after `--`.
        let dash_dash = argv.iter().position(|s| s == "--").expect("-- present");
        assert_eq!(argv[dash_dash + 1], "npx");
        assert_eq!(argv[dash_dash + 2], "-y");
        assert_eq!(argv[dash_dash + 3], "@ctx/server");
    }

    #[test]
    fn mcp_stdio_argv_deny_network_unshares() {
        let argv = build_bwrap_argv_for_mcp_stdio("cmd", &[], true);
        assert!(argv.contains(&"--unshare-net".to_string()));
    }
}
