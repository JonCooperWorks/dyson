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
// The after() method preserves OsSandbox's output truncation behavior.
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::Result;
#[cfg(target_os = "linux")]
use crate::sandbox::os::build_bwrap_command_from_policy;
#[cfg(target_os = "macos")]
use crate::sandbox::os::build_seatbelt_command_from_policy;
use crate::sandbox::policy::{Access, PathAccess, PolicyTable, SandboxPolicy, ToolPolicyConfig};
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::tool::{ToolContext, ToolOutput};

/// Maximum tool output size (characters) before truncation.
const MAX_OUTPUT_CHARS: usize = 100_000;

// ---------------------------------------------------------------------------
// PolicySandbox
// ---------------------------------------------------------------------------

/// Sandbox that enforces per-tool capability policies.
///
/// Combines application-level checks (for Rust-native tools) with
/// OS-level sandboxing (for bash).  Subsumes `OsSandbox` — when active,
/// `OsSandbox` is not needed in the composite pipeline.
pub struct PolicySandbox {
    /// Resolved policy table (exact + glob + defaults).
    policies: PolicyTable,
    /// Working directory for path resolution and bash sandboxing.
    working_dir: PathBuf,
}

impl PolicySandbox {
    /// Create from parsed config.
    pub fn new(
        tool_policies: &HashMap<String, ToolPolicyConfig>,
        working_dir: &Path,
    ) -> Self {
        let policies = PolicyTable::from_config(tool_policies, working_dir);
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
                check_file_read(tool_name, input, &policy, &self.working_dir)
            }

            // ----- File write tools -----
            "write_file" => check_file_write(tool_name, input, &policy, &self.working_dir),

            // ----- File read + write tools -----
            "edit_file" => check_file_read_write(tool_name, input, &policy, &self.working_dir),

            // ----- Network tools -----
            "web_search" => check_network(tool_name, input, &policy),

            // ----- Internal tools — always allowed -----
            "workspace_view" | "workspace_search" | "workspace_update" | "memory_search" => {
                Ok(SandboxDecision::Allow {
                    input: input.clone(),
                })
            }

            // ----- Unknown tools (including MCP) -----
            _ => check_unknown(tool_name, input, &policy),
        }
    }

    async fn after(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        output: &mut ToolOutput,
    ) -> Result<()> {
        // Truncate oversized output (same behavior as OsSandbox).
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
            build_seatbelt_command_from_policy(command, policy, &working_dir.to_string_lossy());

        tracing::debug!(
            original = command,
            "bash command wrapped in OS sandbox (macOS seatbelt, policy-based)"
        );

        return Ok(SandboxDecision::Allow {
            input: serde_json::json!({ "command": sandboxed }),
        });
    }

    #[cfg(target_os = "linux")]
    {
        let sandboxed =
            build_bwrap_command_from_policy(command, policy, &working_dir.to_string_lossy());

        tracing::debug!(
            original = command,
            "bash command wrapped in OS sandbox (Linux bwrap, policy-based)"
        );

        return Ok(SandboxDecision::Allow {
            input: serde_json::json!({ "command": sandboxed }),
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        tracing::warn!(
            "OS sandbox not available on this platform — running unsandboxed"
        );
        Ok(SandboxDecision::Allow {
            input: input.clone(),
        })
    }
}

/// Check a tool that reads files — validate path against file_read policy.
fn check_file_read(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
    working_dir: &Path,
) -> Result<SandboxDecision> {
    // Extract the file path from the input.
    let path_key = match tool_name {
        "list_files" => "path",
        "search_files" => "path",
        _ => "file_path",
    };

    if let Some(file_path) = input[path_key].as_str() {
        if !check_path_access(&policy.file_read, file_path, working_dir) {
            return Ok(SandboxDecision::Deny {
                reason: format!(
                    "sandbox policy denies file read for '{file_path}' by tool '{tool_name}'"
                ),
            });
        }
    }
    // If no path in input, let the tool itself handle the error.

    Ok(SandboxDecision::Allow {
        input: input.clone(),
    })
}

/// Check a tool that writes files — validate path against file_write policy.
fn check_file_write(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
    working_dir: &Path,
) -> Result<SandboxDecision> {
    if let Some(file_path) = input["file_path"].as_str() {
        if !check_path_access(&policy.file_write, file_path, working_dir) {
            return Ok(SandboxDecision::Deny {
                reason: format!(
                    "sandbox policy denies file write for '{file_path}' by tool '{tool_name}'"
                ),
            });
        }
    }

    Ok(SandboxDecision::Allow {
        input: input.clone(),
    })
}

/// Check a tool that reads AND writes files.
fn check_file_read_write(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
    working_dir: &Path,
) -> Result<SandboxDecision> {
    if let Some(file_path) = input["file_path"].as_str() {
        if !check_path_access(&policy.file_read, file_path, working_dir) {
            return Ok(SandboxDecision::Deny {
                reason: format!(
                    "sandbox policy denies file read for '{file_path}' by tool '{tool_name}'"
                ),
            });
        }
        if !check_path_access(&policy.file_write, file_path, working_dir) {
            return Ok(SandboxDecision::Deny {
                reason: format!(
                    "sandbox policy denies file write for '{file_path}' by tool '{tool_name}'"
                ),
            });
        }
    }

    Ok(SandboxDecision::Allow {
        input: input.clone(),
    })
}

/// Check a tool that needs network access.
fn check_network(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
) -> Result<SandboxDecision> {
    if policy.network == Access::Deny {
        return Ok(SandboxDecision::Deny {
            reason: format!("sandbox policy denies network access for tool '{tool_name}'"),
        });
    }

    Ok(SandboxDecision::Allow {
        input: input.clone(),
    })
}

/// Check an unknown tool (including MCP tools).
///
/// MCP tools typically need network access.  We check network policy.
/// File access is denied by default for unknown tools (application-level),
/// but MCP tools run in their own process so file access enforcement
/// would need OS-level sandboxing of the MCP server process itself.
fn check_unknown(
    tool_name: &str,
    input: &serde_json::Value,
    policy: &SandboxPolicy,
) -> Result<SandboxDecision> {
    if policy.network == Access::Deny {
        tracing::debug!(
            tool = tool_name,
            "sandbox policy denies network for unknown tool"
        );
        return Ok(SandboxDecision::Deny {
            reason: format!("sandbox policy denies network access for tool '{tool_name}'"),
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
/// Resolves the path relative to working_dir and checks against the policy.
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

            // Normalize (without requiring the path to exist).
            let normalized = normalize_path(&resolved);

            allowed.iter().any(|prefix| normalized.starts_with(prefix))
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
        for tool in &["workspace_view", "workspace_search", "workspace_update", "memory_search"] {
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
                file_write: Some(crate::sandbox::policy::ToolPolicyPathConfig::RestrictTo(vec![
                    "/tmp/output".into(),
                ])),
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
