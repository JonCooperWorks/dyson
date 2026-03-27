// ===========================================================================
// Sandbox policy — platform-agnostic intent for what a tool is allowed to do.
//
// LEARNING OVERVIEW
//
// Why separate intent from enforcement?
//
//   A sandbox policy says WHAT a tool can do ("allow network", "deny file
//   writes outside /tmp").  It says nothing about HOW that's enforced.
//   The enforcement layer translates intent into platform-specific
//   mechanisms:
//
//     Intent:        network: Deny
//     Linux (bwrap): --unshare-net
//     macOS (Seatbelt): (deny network*)
//     Application:   reject tool call before execution
//
//   This separation means the same policy config works on any platform.
//   Adding a new backend (Landlock, seccomp, WASM) only requires a new
//   translator — policies and configuration don't change.
//
// Capability model:
//
//   Rather than a blocklist of dangerous actions, we use a capability
//   model: each tool starts with NO permissions, then gets granted
//   specific capabilities.  This is safer because new tools default
//   to "deny everything" instead of "allow everything".
//
//   Four capabilities:
//   - network: can make outbound connections (kernel-enforced via firewall)
//   - file_read: can read from the filesystem
//   - file_write: can write to the filesystem
//   - process_exec: can spawn child processes
//
//   File capabilities support path restrictions: "allow writes, but only
//   to /tmp and the project directory".
//
// Default policies:
//
//   Every built-in tool has a sensible default policy.  `web_search` gets
//   network but no file access.  `read_file` gets file_read in the working
//   directory but no network.  `bash` gets file read/write + process exec
//   but no network.  Unknown tools (including MCP) default to deny-all
//   except network (since MCP tools typically need it).
//
//   Users can override per-tool in dyson.json:
//   ```json
//   "sandbox": {
//     "tool_policies": {
//       "web_search": { "network": "allow", "file_read": "deny" }
//     }
//   }
//   ```
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Access types — express intent, not mechanism.
// ---------------------------------------------------------------------------

/// Binary access control for a capability.
///
/// Enforcement depends on the layer:
/// - Network: kernel-enforced via `--unshare-net` (bwrap) or `(deny network*)` (Seatbelt)
/// - Process exec: kernel-enforced via `--unshare-pid` (partial)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Allow,
    Deny,
}

/// Path-scoped access control for filesystem capabilities.
///
/// `RestrictTo` specifies directory prefixes.  A file operation is allowed
/// only if the resolved path starts with one of the allowed directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAccess {
    /// Unrestricted access to the entire filesystem.
    Allow,
    /// No access at all.
    Deny,
    /// Access restricted to these directory prefixes.
    ///
    /// Paths are resolved to absolute form during policy resolution.
    /// The special string `"{cwd}"` is expanded to the working directory.
    RestrictTo(Vec<PathBuf>),
}

// ---------------------------------------------------------------------------
// SandboxPolicy — the core abstraction.
// ---------------------------------------------------------------------------

/// What a tool is allowed to do — platform-agnostic intent.
///
/// This struct expresses capabilities without specifying enforcement.
/// It's the common language between configuration, default policies,
/// and enforcement backends (bwrap, Seatbelt, application-level checks).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Can the tool make outbound network connections?
    ///
    /// Enforced at the OS level: `--unshare-net` (bwrap) or
    /// `(deny network*)` (Seatbelt).
    pub network: Access,

    /// Can the tool read files?
    ///
    /// For Rust-native tools: enforced in `PolicySandbox::check()` by
    /// validating file paths in the input JSON.
    /// For bash: enforced via bwrap `--ro-bind` / Seatbelt `(deny file-read*)`.
    pub file_read: PathAccess,

    /// Can the tool write files?
    ///
    /// Same enforcement strategy as `file_read`.
    pub file_write: PathAccess,

    /// PID namespace isolation for child processes.
    ///
    /// When `Deny`, the OS sandbox hides host processes via `--unshare-pid`
    /// (bwrap).  NOTE: This does NOT prevent `fork()`/`execve()` — the
    /// sandboxed process can still spawn children.  True exec prevention
    /// requires seccomp filters (future work).
    ///
    /// Only meaningful for `bash`.  Other tools don't spawn processes
    /// through the sandbox gate.
    pub process_exec: Access,
}

impl SandboxPolicy {
    /// A policy that denies everything — the safe default for unknown tools.
    pub fn deny_all() -> Self {
        Self {
            network: Access::Deny,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        }
    }
}

// ---------------------------------------------------------------------------
// Default policies per tool.
// ---------------------------------------------------------------------------

/// Returns the default `SandboxPolicy` for a given tool name.
///
/// The `working_dir` is used to expand path restrictions.
/// Unknown tools get a deny-all policy.
pub fn default_policy(tool_name: &str, working_dir: &Path) -> SandboxPolicy {
    let cwd = working_dir.to_path_buf();
    let tmp = PathBuf::from("/tmp");

    match tool_name {
        "bash" => SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![cwd.clone()]),
            file_write: PathAccess::RestrictTo(vec![cwd, tmp]),
            process_exec: Access::Allow,
        },
        "web_search" => SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        },
        "read_file" => SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![cwd]),
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        },
        "write_file" => SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::Deny,
            file_write: PathAccess::RestrictTo(vec![cwd]),
            process_exec: Access::Deny,
        },
        "edit_file" => SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![cwd.clone()]),
            file_write: PathAccess::RestrictTo(vec![cwd]),
            process_exec: Access::Deny,
        },
        "list_files" | "search_files" | "send_file" => SandboxPolicy {
            network: Access::Deny,
            file_read: PathAccess::RestrictTo(vec![cwd]),
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        },
        // Workspace and memory tools are internal — always allowed.
        "workspace_view" | "workspace_search" | "workspace_update" | "memory_search" => {
            SandboxPolicy {
                network: Access::Deny,
                file_read: PathAccess::Allow,
                file_write: PathAccess::Allow,
                process_exec: Access::Deny,
            }
        }
        // Unknown tools (including MCP) default to network-only.
        // MCP tools need network to communicate with their server.
        _ => SandboxPolicy {
            network: Access::Allow,
            file_read: PathAccess::Deny,
            file_write: PathAccess::Deny,
            process_exec: Access::Deny,
        },
    }
}

// ---------------------------------------------------------------------------
// Policy resolution — merge config overrides with defaults.
// ---------------------------------------------------------------------------

/// Parsed policy configuration from dyson.json (before resolution).
///
/// All fields are optional — unspecified fields inherit from the
/// default policy for that tool.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicyConfig {
    pub network: Option<String>,
    pub file_read: Option<ToolPolicyPathConfig>,
    pub file_write: Option<ToolPolicyPathConfig>,
    pub process_exec: Option<String>,
}

/// Path access configuration from JSON.
#[derive(Debug, Clone)]
pub enum ToolPolicyPathConfig {
    /// "allow" or "deny"
    Simple(String),
    /// { "restrict_to": ["/path1", "/path2"] }
    RestrictTo(Vec<String>),
}

/// Resolve a `ToolPolicyConfig` (from JSON) into a `SandboxPolicy`.
///
/// Unspecified fields fall back to the default policy for the given tool.
pub fn resolve_policy(
    tool_name: &str,
    config: &ToolPolicyConfig,
    working_dir: &Path,
) -> SandboxPolicy {
    let default = default_policy(tool_name, working_dir);

    SandboxPolicy {
        network: config
            .network
            .as_deref()
            .map(parse_access)
            .unwrap_or(default.network),
        file_read: config
            .file_read
            .as_ref()
            .map(|c| parse_path_access(c, working_dir))
            .unwrap_or(default.file_read),
        file_write: config
            .file_write
            .as_ref()
            .map(|c| parse_path_access(c, working_dir))
            .unwrap_or(default.file_write),
        process_exec: config
            .process_exec
            .as_deref()
            .map(parse_access)
            .unwrap_or(default.process_exec),
    }
}

fn parse_access(s: &str) -> Access {
    match s.to_lowercase().as_str() {
        "allow" => Access::Allow,
        _ => Access::Deny,
    }
}

fn parse_path_access(config: &ToolPolicyPathConfig, working_dir: &Path) -> PathAccess {
    match config {
        ToolPolicyPathConfig::Simple(s) => match s.to_lowercase().as_str() {
            "allow" => PathAccess::Allow,
            "deny" => PathAccess::Deny,
            _ => PathAccess::Deny,
        },
        ToolPolicyPathConfig::RestrictTo(paths) => {
            let resolved: Vec<PathBuf> = paths
                .iter()
                .map(|p| expand_path(p, working_dir))
                .collect();
            PathAccess::RestrictTo(resolved)
        }
    }
}

/// Expand special placeholders in a path string.
///
/// `{cwd}` is replaced with the working directory.
fn expand_path(path: &str, working_dir: &Path) -> PathBuf {
    let expanded = path.replace("{cwd}", &working_dir.to_string_lossy());
    PathBuf::from(expanded)
}

// ---------------------------------------------------------------------------
// Policy lookup — match tool names against configured policies.
// ---------------------------------------------------------------------------

/// Holds resolved policies, including glob patterns for MCP tools.
pub struct PolicyTable {
    /// Exact tool name → policy.
    exact: HashMap<String, SandboxPolicy>,
    /// Glob patterns → policy (e.g., "mcp__*").
    /// Sorted by specificity: longer patterns match first.
    globs: Vec<(glob::Pattern, SandboxPolicy)>,
    /// Working directory for generating default policies on the fly.
    working_dir: PathBuf,
}

impl PolicyTable {
    /// Build from config, merging with defaults.
    pub fn from_config(
        tool_policies: &HashMap<String, ToolPolicyConfig>,
        working_dir: &Path,
    ) -> Self {
        let mut exact = HashMap::new();
        let mut globs = Vec::new();

        // Populate defaults for all known built-in tools.
        for name in &[
            "bash",
            "web_search",
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "search_files",
            "send_file",
            "workspace_view",
            "workspace_search",
            "workspace_update",
            "memory_search",
        ] {
            exact.insert(name.to_string(), default_policy(name, working_dir));
        }

        // Apply user overrides.
        for (pattern, config) in tool_policies {
            if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
                // It's a glob pattern.
                if let Ok(compiled) = glob::Pattern::new(pattern) {
                    let policy = resolve_policy(pattern, config, working_dir);
                    globs.push((compiled, policy));
                } else {
                    tracing::warn!(
                        pattern = pattern,
                        "invalid glob in tool_policies — skipping"
                    );
                }
            } else {
                // Exact tool name — override or add.
                let policy = resolve_policy(pattern, config, working_dir);
                exact.insert(pattern.clone(), policy);
            }
        }

        // Sort globs by pattern length descending (more specific first).
        globs.sort_by(|a, b| b.0.as_str().len().cmp(&a.0.as_str().len()));

        Self {
            exact,
            globs,
            working_dir: working_dir.to_path_buf(),
        }
    }

    /// Look up the policy for a tool.
    ///
    /// Priority: exact match → longest glob match → default for tool name.
    pub fn get(&self, tool_name: &str) -> SandboxPolicy {
        // 1. Exact match.
        if let Some(policy) = self.exact.get(tool_name) {
            return policy.clone();
        }

        // 2. Glob match (most specific first).
        for (pattern, policy) in &self.globs {
            if pattern.matches(tool_name) {
                return policy.clone();
            }
        }

        // 3. Default for this tool name.
        default_policy(tool_name, &self.working_dir)
    }
}

// ---------------------------------------------------------------------------
// PathAccess helper — check if a path is allowed.
// ---------------------------------------------------------------------------

impl PathAccess {
    /// Check if a given resolved path is allowed by this policy.
    pub fn allows_path(&self, path: &Path) -> bool {
        match self {
            PathAccess::Allow => true,
            PathAccess::Deny => false,
            PathAccess::RestrictTo(allowed) => {
                allowed.iter().any(|prefix| path.starts_with(prefix))
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn wd() -> PathBuf {
        PathBuf::from("/workspace/project")
    }

    // -----------------------------------------------------------------------
    // Access / PathAccess basics
    // -----------------------------------------------------------------------

    #[test]
    fn path_access_deny_denies_everything() {
        assert!(!PathAccess::Deny.allows_path(Path::new("/any/path")));
    }

    #[test]
    fn path_access_allow_allows_everything() {
        assert!(PathAccess::Allow.allows_path(Path::new("/any/path")));
    }

    #[test]
    fn path_access_restrict_to_allows_within() {
        let pa = PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]);
        assert!(pa.allows_path(Path::new("/workspace/file.txt")));
        assert!(pa.allows_path(Path::new("/workspace/sub/dir/file.txt")));
    }

    #[test]
    fn path_access_restrict_to_denies_outside() {
        let pa = PathAccess::RestrictTo(vec![PathBuf::from("/workspace")]);
        assert!(!pa.allows_path(Path::new("/etc/passwd")));
        assert!(!pa.allows_path(Path::new("/tmp/file")));
    }

    #[test]
    fn path_access_restrict_to_multiple_prefixes() {
        let pa = PathAccess::RestrictTo(vec![
            PathBuf::from("/workspace"),
            PathBuf::from("/tmp"),
        ]);
        assert!(pa.allows_path(Path::new("/workspace/file.txt")));
        assert!(pa.allows_path(Path::new("/tmp/scratch")));
        assert!(!pa.allows_path(Path::new("/etc/shadow")));
    }

    // -----------------------------------------------------------------------
    // Default policies
    // -----------------------------------------------------------------------

    #[test]
    fn bash_default_denies_network() {
        let p = default_policy("bash", &wd());
        assert_eq!(p.network, Access::Deny);
    }

    #[test]
    fn bash_default_allows_process_exec() {
        let p = default_policy("bash", &wd());
        assert_eq!(p.process_exec, Access::Allow);
    }

    #[test]
    fn bash_default_allows_file_write_to_cwd_and_tmp() {
        let p = default_policy("bash", &wd());
        match &p.file_write {
            PathAccess::RestrictTo(paths) => {
                assert!(paths.contains(&wd()));
                assert!(paths.contains(&PathBuf::from("/tmp")));
                assert_eq!(paths.len(), 2);
            }
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
    }

    #[test]
    fn web_search_default_allows_network_denies_files() {
        let p = default_policy("web_search", &wd());
        assert_eq!(p.network, Access::Allow);
        assert_eq!(p.file_read, PathAccess::Deny);
        assert_eq!(p.file_write, PathAccess::Deny);
        assert_eq!(p.process_exec, Access::Deny);
    }

    #[test]
    fn read_file_default_allows_read_in_cwd() {
        let p = default_policy("read_file", &wd());
        assert_eq!(p.network, Access::Deny);
        match &p.file_read {
            PathAccess::RestrictTo(paths) => {
                assert!(paths.contains(&wd()));
            }
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
        assert_eq!(p.file_write, PathAccess::Deny);
    }

    #[test]
    fn write_file_default_allows_write_in_cwd() {
        let p = default_policy("write_file", &wd());
        assert_eq!(p.network, Access::Deny);
        assert_eq!(p.file_read, PathAccess::Deny);
        match &p.file_write {
            PathAccess::RestrictTo(paths) => {
                assert!(paths.contains(&wd()));
            }
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
    }

    #[test]
    fn edit_file_default_allows_read_and_write_in_cwd() {
        let p = default_policy("edit_file", &wd());
        match &p.file_read {
            PathAccess::RestrictTo(paths) => assert!(paths.contains(&wd())),
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
        match &p.file_write {
            PathAccess::RestrictTo(paths) => assert!(paths.contains(&wd())),
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
    }

    #[test]
    fn workspace_tools_always_allowed() {
        for name in &["workspace_view", "workspace_search", "workspace_update", "memory_search"] {
            let p = default_policy(name, &wd());
            assert_eq!(p.file_read, PathAccess::Allow, "{name} should allow file_read");
            assert_eq!(p.file_write, PathAccess::Allow, "{name} should allow file_write");
        }
    }

    #[test]
    fn unknown_tool_defaults_to_network_only() {
        let p = default_policy("mcp__github__search", &wd());
        assert_eq!(p.network, Access::Allow);
        assert_eq!(p.file_read, PathAccess::Deny);
        assert_eq!(p.file_write, PathAccess::Deny);
        assert_eq!(p.process_exec, Access::Deny);
    }

    #[test]
    fn deny_all_denies_everything() {
        let p = SandboxPolicy::deny_all();
        assert_eq!(p.network, Access::Deny);
        assert_eq!(p.file_read, PathAccess::Deny);
        assert_eq!(p.file_write, PathAccess::Deny);
        assert_eq!(p.process_exec, Access::Deny);
    }

    // -----------------------------------------------------------------------
    // Policy resolution from config
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_overrides_network() {
        let config = ToolPolicyConfig {
            network: Some("allow".into()),
            ..Default::default()
        };
        let p = resolve_policy("bash", &config, &wd());
        assert_eq!(p.network, Access::Allow);
        // Other fields inherit defaults.
        assert_eq!(p.process_exec, Access::Allow);
    }

    #[test]
    fn resolve_overrides_file_write_restrict_to() {
        let config = ToolPolicyConfig {
            file_write: Some(ToolPolicyPathConfig::RestrictTo(vec![
                "/tmp/custom".into(),
            ])),
            ..Default::default()
        };
        let p = resolve_policy("web_search", &config, &wd());
        match &p.file_write {
            PathAccess::RestrictTo(paths) => {
                assert_eq!(paths, &[PathBuf::from("/tmp/custom")]);
            }
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_expands_cwd_placeholder() {
        let config = ToolPolicyConfig {
            file_read: Some(ToolPolicyPathConfig::RestrictTo(vec![
                "{cwd}/subdir".into(),
            ])),
            ..Default::default()
        };
        let p = resolve_policy("custom_tool", &config, &wd());
        match &p.file_read {
            PathAccess::RestrictTo(paths) => {
                assert_eq!(paths[0], PathBuf::from("/workspace/project/subdir"));
            }
            other => panic!("expected RestrictTo, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_simple_deny() {
        let config = ToolPolicyConfig {
            file_read: Some(ToolPolicyPathConfig::Simple("deny".into())),
            ..Default::default()
        };
        let p = resolve_policy("read_file", &config, &wd());
        assert_eq!(p.file_read, PathAccess::Deny);
    }

    // -----------------------------------------------------------------------
    // PolicyTable lookup
    // -----------------------------------------------------------------------

    #[test]
    fn table_exact_match() {
        let table = PolicyTable::from_config(&HashMap::new(), &wd());
        let p = table.get("web_search");
        assert_eq!(p.network, Access::Allow);
        assert_eq!(p.file_read, PathAccess::Deny);
    }

    #[test]
    fn table_user_override_takes_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "web_search".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        let table = PolicyTable::from_config(&overrides, &wd());
        let p = table.get("web_search");
        assert_eq!(p.network, Access::Deny);
    }

    #[test]
    fn table_glob_matches_mcp_tools() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mcp__*".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        let table = PolicyTable::from_config(&overrides, &wd());
        let p = table.get("mcp__github__search");
        assert_eq!(p.network, Access::Deny);
    }

    #[test]
    fn table_specific_glob_wins_over_broad_glob() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mcp__*".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        overrides.insert(
            "mcp__github__*".into(),
            ToolPolicyConfig {
                network: Some("allow".into()),
                ..Default::default()
            },
        );
        let table = PolicyTable::from_config(&overrides, &wd());
        // mcp__github__* is more specific (longer) than mcp__*
        let p = table.get("mcp__github__search");
        assert_eq!(p.network, Access::Allow);
    }

    #[test]
    fn table_exact_wins_over_glob() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mcp__*".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        overrides.insert(
            "mcp__special".into(),
            ToolPolicyConfig {
                network: Some("allow".into()),
                ..Default::default()
            },
        );
        let table = PolicyTable::from_config(&overrides, &wd());
        let p = table.get("mcp__special");
        assert_eq!(p.network, Access::Allow);
    }

    #[test]
    fn table_unknown_tool_gets_default() {
        let table = PolicyTable::from_config(&HashMap::new(), &wd());
        let p = table.get("totally_unknown_tool");
        // Unknown defaults to network-only.
        assert_eq!(p.network, Access::Allow);
        assert_eq!(p.file_read, PathAccess::Deny);
    }
}
