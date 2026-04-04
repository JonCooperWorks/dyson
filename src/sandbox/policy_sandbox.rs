// ===========================================================================
// PolicySandbox — enforce per-tool sandbox policies.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements a Sandbox that enforces per-tool capability policies at
//   two levels:
//
//   1. Application-level: For Rust-native tools (read_file, write_file,
//      web_search, etc.), inspect the tool's input JSON and deny calls
//      that violate the policy.  This is possible because we know the
//      input schema for each built-in tool.
//
//   2. OS-level: For bash, translate the policy into bwrap flags (Linux)
//      or Seatbelt S-expressions (macOS) that the kernel enforces.
//
// Why two layers?
//
//   Rust-native tools are our code — the LLM can't bypass the check()
//   gate, so application-level enforcement is sufficient.  Bash is
//   different: the LLM controls an arbitrary command string that runs
//   in a shell, so we need kernel-level enforcement to prevent breakout.
//
// How check() dispatches:
//
//   1. Look up the policy for the tool (exact → glob → default)
//   2. For file tools: extract the path from input, validate against policy
//   3. For network tools: check if network capability is granted
//   4. For bash: generate bwrap/seatbelt wrapper command
//   5. For workspace/memory tools: always allow (internal tools)
//
// The after() method truncates oversized tool output to protect the
// context window.
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::sandbox::os::build_bwrap_command_from_policy;
#[cfg(target_os = "macos")]
use crate::sandbox::os::build_container_command_from_policy;
use crate::sandbox::policy::{Access, PathAccess, PolicyTable, SandboxPolicy, ToolPolicyConfig};
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::{ToolContext, ToolOutput};

use super::MAX_OUTPUT_CHARS;

// ---------------------------------------------------------------------------
// PolicySandbox
// ---------------------------------------------------------------------------

/// Sandbox that enforces per-tool capability policies.
///
/// Combines application-level checks (for Rust-native tools) with
/// OS-level sandboxing (for bash via bwrap on Linux or Apple Containers
/// on macOS).
pub struct PolicySandbox {
    /// Resolved policy table (exact + glob + defaults).
    policies: PolicyTable,
    /// Working directory for path resolution and bash sandboxing.
    working_dir: PathBuf,
}

impl PolicySandbox {
    /// Create from parsed config.
    ///
    /// Pre-canonicalizes all `RestrictTo` paths in the policy table so that
    /// `check_path_access` only needs to canonicalize the input path on each
    /// call, not the (static) allowed prefixes.
    pub fn new(tool_policies: &HashMap<String, ToolPolicyConfig>, working_dir: &Path) -> Self {
        let mut policies = PolicyTable::from_config(tool_policies, working_dir);
        policies.pre_canonicalize_paths();
        Self {
            policies,
            working_dir: working_dir.to_path_buf(),
        }
    }
}

#[async_trait]
impl Sandbox for PolicySandbox {
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        let policy = self.policies.get(tool_name);

        match tool_name {
            // ----- Bash: OS-level enforcement -----
            "bash" => check_bash(input, &policy, &self.working_dir),

            // ----- File read tools -----
            "read_file" | "list_files" | "search_files" | "send_file" => {
                check_file_access(tool_name, input, &policy, &self.working_dir, true, false)
            }

            // ----- File write tools -----
            "write_file" => {
                check_file_access(tool_name, input, &policy, &self.working_dir, false, true)
            }

            // ----- File read + write tools -----
            "edit_file" => {
                check_file_access(tool_name, input, &policy, &self.working_dir, true, true)
            }

            // ----- Internal tools — always allowed -----
            "workspace_view" | "workspace_search" | "workspace_update" | "memory_search" => {
                Ok(SandboxDecision::Allow {
                    input: input.clone(),
                })
            }

            // ----- Network tools + unknown tools (including MCP) -----
            _ => {
                if policy.network == Access::Deny {
                    tracing::debug!(
                        tool = tool_name,
                        "sandbox policy denies network access"
                    );
                    return Ok(SandboxDecision::Deny {
                        reason: format!(
                            "sandbox policy denies network access for tool '{tool_name}'"
                        ),
                    });
                }
                Ok(SandboxDecision::Allow {
                    input: input.clone(),
                })
            }
        }
    }

    async fn after(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        // Truncate oversized output to protect the LLM context window.
        if output.content.len() > MAX_OUTPUT_CHARS {
            let original_len = output.content.len();
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
// Per-tool check functions
// ---------------------------------------------------------------------------

/// Check bash: generate OS-level sandbox wrapper from policy.
fn check_bash(
    input: &serde_json::Value,
    policy: &SandboxPolicy,
    working_dir: &Path,
) -> Result<SandboxDecision> {
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
        let sandboxed =
            build_container_command_from_policy(command, policy, &working_dir.to_string_lossy());

        tracing::debug!(
            original = command,
            "bash command wrapped in Apple Container"
        );

        Ok(SandboxDecision::Allow {
            input: serde_json::json!({ "command": sandboxed }),
        })
    }

    #[cfg(target_os = "linux")]
    {
        let sandboxed =
            build_bwrap_command_from_policy(command, policy, &working_dir.to_string_lossy());

        tracing::debug!(
            original = command,
            "bash command wrapped in OS sandbox (Linux bwrap, policy-based)"
        );

        Ok(SandboxDecision::Allow {
            input: serde_json::json!({ "command": sandboxed }),
        })
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        tracing::warn!("OS sandbox not available on this platform — running unsandboxed");
        Ok(SandboxDecision::Allow {
            input: input.clone(),
        })
    }
}

/// Check a tool that accesses files — validate path against read/write policies.
///
/// `check_read` and `check_write` control which capabilities are validated.
/// This consolidates the formerly separate read, write, and read+write checks.
fn check_file_access(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
    working_dir: &Path,
    check_read: bool,
    check_write: bool,
) -> Result<SandboxDecision> {
    let path_key = match tool_name {
        "list_files" | "search_files" => "path",
        _ => "file_path",
    };

    let file_path = match input[path_key].as_str() {
        Some(p) => p,
        None => {
            return Ok(SandboxDecision::Deny {
                reason: format!("missing '{path_key}' field in {tool_name} input"),
            });
        }
    };

    if check_read && !check_path_access(&policy.file_read, file_path, working_dir) {
        return Ok(SandboxDecision::Deny {
            reason: format!(
                "sandbox policy denies file read for '{file_path}' by tool '{tool_name}'"
            ),
        });
    }

    if check_write && !check_path_access(&policy.file_write, file_path, working_dir) {
        return Ok(SandboxDecision::Deny {
            reason: format!(
                "sandbox policy denies file write for '{file_path}' by tool '{tool_name}'"
            ),
        });
    }

    Ok(SandboxDecision::Allow {
        input: input.clone(),
    })
}

// ---------------------------------------------------------------------------
// Path checking helper
// ---------------------------------------------------------------------------

/// Check if a file path (from tool input) is allowed by a PathAccess policy.
///
/// Resolves the path relative to working_dir, canonicalizes to resolve
/// symlinks, and checks against the policy.  Falls back to lexical
/// normalization for paths that don't exist yet.
fn check_path_access(access: &PathAccess, file_path: &str, working_dir: &Path) -> bool {
    match access {
        PathAccess::Allow => true,
        PathAccess::Deny => false,
        PathAccess::RestrictTo(allowed) => {
            // Resolve to absolute path.
            let resolved = if Path::new(file_path).is_absolute() {
                PathBuf::from(file_path)
            } else {
                working_dir.join(file_path)
            };

            // Canonicalize to resolve symlinks.  For paths that don't exist
            // yet, canonicalize the nearest existing ancestor and re-append
            // remaining components (same pattern as tool/mod.rs).
            let canonical = resolve_canonical(&resolved);

            // Allowed prefixes are pre-canonicalized at PolicySandbox
            // construction time, so we only need a simple starts_with check.
            allowed.iter().any(|prefix| canonical.starts_with(prefix))
        }
    }
}

/// Resolve a path to its canonical form, following symlinks.
///
/// Strategy:
/// 1. Normalize lexically first (resolve `.` and `..` without filesystem access).
/// 2. Try `canonicalize()` on the normalized path (resolves symlinks).
/// 3. If the path doesn't exist: walk up to the nearest existing ancestor,
///    canonicalize that, then re-append the remaining components.
/// 4. If no ancestor exists: return the lexically normalized path.
pub(crate) fn resolve_canonical(path: &Path) -> PathBuf {
    // Always normalize first to resolve .. and . components.
    let normalized = normalize_path(path);

    // Fast path: the normalized path exists — canonicalize resolves symlinks.
    if let Ok(canon) = std::fs::canonicalize(&normalized) {
        return canon;
    }

    // Slow path: walk up to find an existing ancestor.
    let mut ancestor = normalized.clone();
    loop {
        if !ancestor.pop() {
            // Reached filesystem root without finding an existing dir.
            return normalized;
        }
        if ancestor.exists() {
            if let Ok(canon) = std::fs::canonicalize(&ancestor)
                && let Ok(suffix) = normalized.strip_prefix(&ancestor)
            {
                return canon.join(suffix);
            }
            return normalized;
        }
    }
}

/// Normalize a path by resolving `.` and `..` components without touching
/// the filesystem (unlike `canonicalize()` which requires the path to exist).
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            _ => {
                components.push(component);
            }
        }
    }
    components.iter().collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    fn wd() -> PathBuf {
        PathBuf::from("/workspace/project")
    }

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: wd(),
            env: std::collections::HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: None,
            depth: 0,
        }
    }

    fn sandbox(overrides: HashMap<String, ToolPolicyConfig>) -> PolicySandbox {
        PolicySandbox::new(&overrides, &wd())
    }

    fn sandbox_default() -> PolicySandbox {
        sandbox(HashMap::new())
    }

    // -----------------------------------------------------------------------
    // normalize_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_removes_parent_dir() {
        let p = normalize_path(Path::new("/workspace/project/../secret"));
        assert_eq!(p, PathBuf::from("/workspace/secret"));
    }

    #[test]
    fn normalize_removes_cur_dir() {
        let p = normalize_path(Path::new("/workspace/./project/./file"));
        assert_eq!(p, PathBuf::from("/workspace/project/file"));
    }

    #[test]
    fn normalize_complex_traversal() {
        let p = normalize_path(Path::new("/workspace/project/sub/../../etc/passwd"));
        assert_eq!(p, PathBuf::from("/workspace/etc/passwd"));
    }

    // -----------------------------------------------------------------------
    // check_path_access tests
    // -----------------------------------------------------------------------

    #[test]
    fn path_access_allow_passes() {
        assert!(check_path_access(&PathAccess::Allow, "/anything", &wd()));
    }

    #[test]
    fn path_access_deny_blocks() {
        assert!(!check_path_access(&PathAccess::Deny, "/anything", &wd()));
    }

    #[test]
    fn path_access_restrict_allows_within() {
        let access = PathAccess::RestrictTo(vec![wd()]);
        assert!(check_path_access(&access, "file.txt", &wd()));
        assert!(check_path_access(&access, "sub/dir/file.txt", &wd()));
    }

    #[test]
    fn path_access_restrict_denies_outside() {
        let access = PathAccess::RestrictTo(vec![wd()]);
        assert!(!check_path_access(&access, "/etc/passwd", &wd()));
    }

    #[test]
    fn path_access_restrict_catches_traversal() {
        let access = PathAccess::RestrictTo(vec![wd()]);
        // ../etc/passwd resolves to /workspace/etc/passwd — outside /workspace/project
        assert!(!check_path_access(&access, "../etc/passwd", &wd()));
    }

    #[test]
    fn path_access_restrict_multiple_prefixes() {
        let access = PathAccess::RestrictTo(vec![wd(), PathBuf::from("/tmp")]);
        assert!(check_path_access(&access, "file.txt", &wd()));
        assert!(check_path_access(&access, "/tmp/scratch", &wd()));
        assert!(!check_path_access(&access, "/etc/passwd", &wd()));
    }

    #[test]
    fn path_access_restrict_catches_symlink() {
        // Create a symlink inside a temp dir that points outside the allowed area.
        let tmp = std::env::temp_dir().join("dyson_sandbox_symlink_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let target = std::env::temp_dir().join("dyson_sandbox_symlink_target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("secret.txt"), "secret").unwrap();

        // Create symlink: tmp/link → target (which is outside tmp)
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, tmp.join("link")).unwrap();

        let access = PathAccess::RestrictTo(vec![tmp.clone()]);
        // The symlink resolves outside the allowed directory.
        let symlink_path = tmp.join("link").join("secret.txt");
        assert!(
            !check_path_access(&access, symlink_path.to_str().unwrap(), &tmp),
            "symlink escaping allowed directory should be denied"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&target);
    }

    // -----------------------------------------------------------------------
    // Bash tool checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn bash_empty_command_denied() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"command": ""});
        let decision = s.check("bash", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn bash_missing_command_denied() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({});
        let decision = s.check("bash", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn bash_valid_command_wrapped() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"command": "ls -la"});
        let decision = s.check("bash", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Allow { input } => {
                let cmd = input["command"].as_str().unwrap();
                assert!(cmd.contains("ls -la"));
                #[cfg(target_os = "linux")]
                assert!(cmd.contains("bwrap"));
                #[cfg(target_os = "macos")]
                assert!(cmd.contains("sandbox-exec"));
            }
            other => panic!("expected Allow, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Read file checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_file_allowed_in_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "src/main.rs"});
        let decision = s.check("read_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn read_file_denied_outside_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "/etc/passwd"});
        let decision = s.check("read_file", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Deny { reason } => {
                assert!(reason.contains("denies file read"));
                assert!(reason.contains("/etc/passwd"));
            }
            other => panic!("expected Deny, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_file_denied_traversal() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "../../../etc/shadow"});
        let decision = s.check("read_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn read_file_denied_when_policy_denies() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "read_file".into(),
            ToolPolicyConfig {
                file_read: Some(crate::sandbox::policy::ToolPolicyPathConfig::Simple(
                    "deny".into(),
                )),
                ..Default::default()
            },
        );
        let s = sandbox(overrides);
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "src/main.rs"});
        let decision = s.check("read_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // Write file checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn write_file_allowed_in_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "output.txt", "content": "hello"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn write_file_denied_outside_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "/etc/crontab", "content": "evil"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn write_file_denied_traversal() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "../../etc/passwd", "content": "x"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // Edit file checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn edit_file_allowed_in_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "src/lib.rs", "content": "updated"});
        let decision = s.check("edit_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn edit_file_denied_outside_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"file_path": "/root/.bashrc", "content": "evil"});
        let decision = s.check("edit_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // Missing path field checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_file_denied_when_path_missing() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"content": "no path here"});
        let decision = s.check("read_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn write_file_denied_when_path_missing() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"content": "no path here"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn edit_file_denied_when_path_missing() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"content": "no path here"});
        let decision = s.check("edit_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // Web search checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn web_search_allowed_by_default() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"query": "rust async"});
        let decision = s.check("web_search", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn web_search_denied_when_network_denied() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "web_search".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        let s = sandbox(overrides);
        let ctx = ctx();
        let input = serde_json::json!({"query": "test"});
        let decision = s.check("web_search", &input, &ctx).await.unwrap();
        match decision {
            SandboxDecision::Deny { reason } => {
                assert!(reason.contains("denies network"));
            }
            other => panic!("expected Deny, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Workspace tools always allowed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn workspace_tools_always_pass() {
        let s = sandbox_default();
        let ctx = ctx();
        for tool in &[
            "workspace_view",
            "workspace_search",
            "workspace_update",
            "memory_search",
        ] {
            let input = serde_json::json!({});
            let decision = s.check(tool, &input, &ctx).await.unwrap();
            assert!(
                matches!(decision, SandboxDecision::Allow { .. }),
                "{tool} should always be allowed"
            );
        }
    }

    // -----------------------------------------------------------------------
    // MCP / unknown tool checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_tool_allowed_by_default() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"query": "test"});
        let decision = s.check("mcp__github__search", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn mcp_tool_denied_when_network_denied_via_glob() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mcp__*".into(),
            ToolPolicyConfig {
                network: Some("deny".into()),
                ..Default::default()
            },
        );
        let s = sandbox(overrides);
        let ctx = ctx();
        let input = serde_json::json!({});
        let decision = s.check("mcp__github__search", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn mcp_specific_glob_overrides_broad() {
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
        let s = sandbox(overrides);
        let ctx = ctx();

        // github MCP tool should be allowed (specific glob).
        let decision = s
            .check("mcp__github__search", &serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));

        // Other MCP tool should be denied (broad glob).
        let decision = s
            .check("mcp__slack__post", &serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // after() output truncation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn after_truncates_oversized_output() {
        let s = sandbox_default();
        let input = serde_json::json!({});
        let big = "a".repeat(1000) + "\n";
        let content = big.repeat(200); // 200K+ chars
        let original_len = content.len();
        let mut output = ToolOutput::success(content);

        s.after("some_tool", &input, &mut output).await.unwrap();
        assert!(output.content.len() < original_len);
        assert!(output.content.contains("[output truncated by sandbox"));
    }

    #[tokio::test]
    async fn after_does_not_truncate_small_output() {
        let s = sandbox_default();
        let input = serde_json::json!({});
        let mut output = ToolOutput::success("small result".to_string());

        s.after("bash", &input, &mut output).await.unwrap();
        assert_eq!(output.content, "small result");
    }

    // -----------------------------------------------------------------------
    // list_files and search_files use "path" key
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_files_uses_path_key() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"path": "/etc"});
        let decision = s.check("list_files", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn list_files_allowed_in_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"path": "src"});
        let decision = s.check("list_files", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));
    }

    #[tokio::test]
    async fn search_files_denied_outside_cwd() {
        let s = sandbox_default();
        let ctx = ctx();
        let input = serde_json::json!({"path": "/var/log"});
        let decision = s.check("search_files", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }

    // -----------------------------------------------------------------------
    // Custom policy with restrict_to paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn custom_write_restriction() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "write_file".into(),
            ToolPolicyConfig {
                file_write: Some(crate::sandbox::policy::ToolPolicyPathConfig::RestrictTo(
                    vec!["/tmp/output".into()],
                )),
                ..Default::default()
            },
        );
        let s = sandbox(overrides);
        let ctx = ctx();

        // Allowed in /tmp/output.
        let input = serde_json::json!({"file_path": "/tmp/output/result.txt", "content": "ok"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Allow { .. }));

        // Denied in working directory (overridden away).
        let input = serde_json::json!({"file_path": "local.txt", "content": "nope"});
        let decision = s.check("write_file", &input, &ctx).await.unwrap();
        assert!(matches!(decision, SandboxDecision::Deny { .. }));
    }
}
