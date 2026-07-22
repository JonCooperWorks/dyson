// ===========================================================================
// Dependency Analyzer — dependency-aware tool call grouping.
//
// Analyzes a batch of tool calls to detect resource conflicts and
// dependencies, then groups them into execution phases (parallel or
// sequential) to ensure correct ordering.
// ===========================================================================

use crate::ToolCall;
use crate::{ResourceAccess as DeclaredAccess, ToolExecutionPlan};

// ---------------------------------------------------------------------------
// ExecutionPhase
// ---------------------------------------------------------------------------

/// A group of tool calls that can be executed together.
#[derive(Debug)]
pub enum ExecutionPhase {
    /// These calls are independent and can run concurrently.
    Parallel(Vec<usize>),
    /// These calls must run one after another in order.
    Sequential(Vec<usize>),
}

// ---------------------------------------------------------------------------
// Resource tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Resource {
    File(String),
    Git,
    /// Conservative shared resource for arbitrary shell side effects.
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AccessKind {
    Read,
    Write,
}

#[derive(Debug)]
struct ResourceAccess {
    resource: Resource,
    kind: AccessKind,
}

// ---------------------------------------------------------------------------
// Git command classification
// ---------------------------------------------------------------------------

/// Git subcommands that mutate repository state.
const GIT_WRITE_COMMANDS: &[&str] = &[
    "git add",
    "git commit",
    "git push",
    "git checkout",
    "git reset",
    "git merge",
    "git rebase",
    "git pull",
    "git stash",
    "git rm",
    "git mv",
    "git tag",
];

/// Git subcommands that only read repository state.
const GIT_READ_COMMANDS: &[&str] = &[
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    "git remote",
];

fn classify_git_command(command: &str) -> Option<AccessKind> {
    let trimmed = command.trim();
    for &cmd in GIT_WRITE_COMMANDS {
        if trimmed.starts_with(cmd) {
            return Some(AccessKind::Write);
        }
    }
    for &cmd in GIT_READ_COMMANDS {
        if trimmed.starts_with(cmd) {
            return Some(AccessKind::Read);
        }
    }
    // Unknown git command — treat as write to be safe.
    if trimmed.starts_with("git ") {
        return Some(AccessKind::Write);
    }
    None
}

/// Classify the small set of shell commands whose lack of side effects is
/// obvious without parsing a shell program. Everything else is a write.
fn classify_shell_command(command: &str) -> AccessKind {
    let trimmed = command.trim();
    if trimmed.contains(['>', '|', ';'])
        || trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains("$(")
        || trimmed.contains('`')
    {
        return AccessKind::Write;
    }
    let executable = trimmed.split_whitespace().next().unwrap_or("");
    if matches!(
        executable,
        "cat" | "echo" | "grep" | "head" | "ls" | "printf" | "pwd" | "rg" | "stat" | "tail" | "wc"
    ) {
        AccessKind::Read
    } else {
        AccessKind::Write
    }
}

// ---------------------------------------------------------------------------
// DependencyAnalyzer
// ---------------------------------------------------------------------------

/// Analyzes tool call dependencies and groups them into execution phases.
pub struct DependencyAnalyzer;

impl DependencyAnalyzer {
    /// Schedule using tool-declared, normalized resource claims.  This is the
    /// production path; unknown tools receive the Tool trait's conservative
    /// global-write plan and therefore never race by accident.
    pub fn analyze_plans(plans: &[ToolExecutionPlan]) -> Vec<ExecutionPhase> {
        let accesses: Vec<Vec<ResourceAccess>> = plans
            .iter()
            .map(|plan| {
                plan.resources
                    .iter()
                    .map(|claim| ResourceAccess {
                        resource: Resource::File(claim.key.clone()),
                        kind: match claim.access {
                            DeclaredAccess::Read => AccessKind::Read,
                            DeclaredAccess::Write => AccessKind::Write,
                        },
                    })
                    .collect()
            })
            .collect();
        phases_for_accesses(&accesses)
    }

    /// Analyze a batch of tool calls and return execution phases.
    ///
    /// The returned phases should be executed in order.  Calls within a
    /// `Parallel` phase can run concurrently; calls within a `Sequential`
    /// phase must run one after another.
    pub fn analyze(calls: &[&ToolCall]) -> Vec<ExecutionPhase> {
        if calls.is_empty() {
            return Vec::new();
        }

        // Step 1: Extract resource accesses for each call.
        let accesses: Vec<Vec<ResourceAccess>> =
            calls.iter().map(|c| extract_resources(c)).collect();

        phases_for_accesses(&accesses)
    }
}

fn phases_for_accesses(accesses: &[Vec<ResourceAccess>]) -> Vec<ExecutionPhase> {
    // Step 2: Build dependency edges.
    // depends_on[i] contains indices of calls that call i depends on.
    let n = accesses.len();
    let mut depends_on: Vec<Vec<usize>> = vec![Vec::new(); n];

    for i in 0..n {
        for j in 0..i {
            if has_dependency(&accesses[j], &accesses[i]) {
                depends_on[i].push(j);
            }
        }
    }

    // Step 3: Build phases via topological layering.
    // Each call's "depth" is 1 + max depth of its dependencies.
    let mut depth = vec![0usize; n];
    for i in 0..n {
        if !depends_on[i].is_empty() {
            depth[i] = depends_on[i]
                .iter()
                .map(|&j| depth[j] + 1)
                .max()
                .unwrap_or(0);
        }
    }

    let max_depth = depth.iter().copied().max().unwrap_or(0);

    let mut phases = Vec::new();
    for d in 0..=max_depth {
        let indices: Vec<usize> = (0..n).filter(|&i| depth[i] == d).collect();
        if indices.is_empty() {
            continue;
        }

        // Check if any calls in this layer conflict with each other.
        let has_conflicts = has_intra_layer_conflicts(&indices, accesses);

        if has_conflicts {
            phases.push(ExecutionPhase::Sequential(indices));
        } else {
            // If every element in this layer has dependencies on prior
            // layers, mark it Sequential to signal ordering constraints.
            let all_dependent = indices.iter().all(|&i| !depends_on[i].is_empty());
            if all_dependent && indices.len() == 1 {
                phases.push(ExecutionPhase::Sequential(indices));
            } else {
                phases.push(ExecutionPhase::Parallel(indices));
            }
        }
    }

    phases
}

// ---------------------------------------------------------------------------
// Resource extraction
// ---------------------------------------------------------------------------

fn extract_resources(call: &ToolCall) -> Vec<ResourceAccess> {
    let mut resources = Vec::new();

    match call.name.as_str() {
        // Reads — pulls one path from the call's `path` argument.
        "read_file" => {
            if let Some(path) = call.input.get("path").and_then(|v| v.as_str()) {
                resources.push(ResourceAccess {
                    resource: Resource::File(path.to_string()),
                    kind: AccessKind::Read,
                });
            }
        }
        // Writes — every file-mutating tool. write_file/edit_file each
        // touch one path; bulk_edit can carry many.
        "write_file" | "edit_file" => {
            if let Some(path) = call.input.get("path").and_then(|v| v.as_str()) {
                resources.push(ResourceAccess {
                    resource: Resource::File(path.to_string()),
                    kind: AccessKind::Write,
                });
            }
        }
        "bulk_edit" => {
            if let Some(edits) = call.input.get("edits").and_then(|v| v.as_array()) {
                for e in edits {
                    if let Some(path) = e.get("path").and_then(|v| v.as_str()) {
                        resources.push(ResourceAccess {
                            resource: Resource::File(path.to_string()),
                            kind: AccessKind::Write,
                        });
                    }
                }
            }
        }
        "bash" => {
            // Shell commands can touch any file, process, or repository state.
            // Without a real shell parser, treating all bash calls as sharing
            // one write resource is the only correctness-preserving default.
            resources.push(ResourceAccess {
                resource: Resource::Shell,
                kind: call
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map_or(AccessKind::Write, classify_shell_command),
            });
            if let Some(command) = call.input.get("command").and_then(|v| v.as_str())
                && let Some(kind) = classify_git_command(command)
            {
                resources.push(ResourceAccess {
                    resource: Resource::Git,
                    kind,
                });
            }
        }
        _ => {}
    }

    resources
}

// ---------------------------------------------------------------------------
// Dependency detection
// ---------------------------------------------------------------------------

/// Returns true if call `later` depends on call `earlier`.
///
/// A dependency exists when:
/// - `earlier` writes to a resource that `later` reads or writes (WAR, WAW)
/// - `earlier` reads a resource that `later` writes (RAW) — to preserve ordering
fn has_dependency(earlier: &[ResourceAccess], later: &[ResourceAccess]) -> bool {
    use std::collections::HashSet;

    // Build sets for O(1) lookup instead of O(n*m) nested iteration.
    let earlier_writes: HashSet<&Resource> = earlier
        .iter()
        .filter(|a| a.kind == AccessKind::Write)
        .map(|a| &a.resource)
        .collect();
    let earlier_reads: HashSet<&Resource> = earlier
        .iter()
        .filter(|a| a.kind == AccessKind::Read)
        .map(|a| &a.resource)
        .collect();

    for l in later {
        if l.kind == AccessKind::Write {
            // RAW or WAW: earlier read or wrote this resource.
            if earlier_writes.contains(&l.resource) || earlier_reads.contains(&l.resource) {
                return true;
            }
        } else {
            // WAR: earlier wrote to resource that later reads.
            if earlier_writes.contains(&l.resource) {
                return true;
            }
        }
    }
    false
}

/// Check if any calls within the same layer conflict with each other.
fn has_intra_layer_conflicts(indices: &[usize], accesses: &[Vec<ResourceAccess>]) -> bool {
    for (i, &idx_a) in indices.iter().enumerate() {
        for &idx_b in &indices[i + 1..] {
            if has_dependency(&accesses[idx_a], &accesses[idx_b]) {
                return true;
            }
        }
    }
    false
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod test_dependency_analyzer {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_independent_calls() {
        let calls = [
            ToolCall::new("bash", json!({"command": "ls"})),
            ToolCall::new("bash", json!({"command": "cat README"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let phases = DependencyAnalyzer::analyze(&refs);
        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], ExecutionPhase::Parallel(_)));
    }

    #[test]
    fn detects_write_then_read_dependency() {
        let calls = [
            ToolCall::new("write_file", json!({"path": "f.txt"})),
            ToolCall::new("read_file", json!({"path": "f.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(DependencyAnalyzer::analyze(&refs).len() >= 2);
    }

    #[test]
    fn detects_same_resource_conflict() {
        let calls = [
            ToolCall::new("write_file", json!({"path": "x.txt"})),
            ToolCall::new("write_file", json!({"path": "x.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let phases = DependencyAnalyzer::analyze(&refs);
        assert!(
            phases
                .iter()
                .any(|p| matches!(p, ExecutionPhase::Sequential(_)))
        );
    }

    #[test]
    fn groups_mixed_calls() {
        let calls = [
            ToolCall::new("bash", json!({"command": "echo A"})),
            ToolCall::new("bash", json!({"command": "echo B"})),
            ToolCall::new("read_file", json!({"path": "unrelated.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(!DependencyAnalyzer::analyze(&refs).is_empty());
    }

    #[test]
    fn infers_git_side_effects() {
        let calls = [
            ToolCall::new("bash", json!({"command": "git add ."})),
            ToolCall::new("bash", json!({"command": "git status"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(DependencyAnalyzer::analyze(&refs).len() >= 2);
    }

    #[test]
    fn unknown_bash_calls_are_serialised_conservatively() {
        let calls = [
            ToolCall::new("bash", json!({"command": "generate-report > report.json"})),
            ToolCall::new("bash", json!({"command": "cat report.json"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(
            DependencyAnalyzer::analyze(&refs).len() >= 2,
            "arbitrary shell commands may share filesystem state and must not race"
        );
    }

    // QP: the analyzer's tool-name match used to read `file_read` /
    // `file_write`, but the actual tools are `read_file` / `write_file`
    // / `edit_file` / `bulk_edit`. The result was that file conflicts
    // weren't detected at all in production — every file-touching tool
    // appeared to have zero resource deps and ran fully concurrently.
    #[test]
    fn detects_write_file_then_read_file_dependency_under_real_tool_names() {
        let calls = [
            ToolCall::new("write_file", json!({"path": "f.txt"})),
            ToolCall::new("read_file", json!({"path": "f.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(
            DependencyAnalyzer::analyze(&refs).len() >= 2,
            "write_file -> read_file of the same path must serialise"
        );
    }

    #[test]
    fn edit_file_conflict_with_read_file_is_serialised() {
        let calls = [
            ToolCall::new("edit_file", json!({"path": "x.txt"})),
            ToolCall::new("read_file", json!({"path": "x.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(
            DependencyAnalyzer::analyze(&refs).len() >= 2,
            "edit_file is a write — must serialise against read_file"
        );
    }

    #[test]
    fn bulk_edit_conflicts_with_subsequent_read_file() {
        let calls = [
            ToolCall::new(
                "bulk_edit",
                json!({"edits": [{"path": "a.txt"}, {"path": "b.txt"}]}),
            ),
            ToolCall::new("read_file", json!({"path": "a.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        assert!(
            DependencyAnalyzer::analyze(&refs).len() >= 2,
            "bulk_edit writes every listed path; subsequent read must wait"
        );
    }

    #[test]
    fn declared_plans_parallelize_reads_and_serialize_writes() {
        let reads = vec![
            ToolExecutionPlan::read("file:/a"),
            ToolExecutionPlan::read("file:/a"),
        ];
        assert!(matches!(
            DependencyAnalyzer::analyze_plans(&reads).as_slice(),
            [ExecutionPhase::Parallel(indices)] if indices.len() == 2
        ));

        let conflict = vec![
            ToolExecutionPlan::write("file:/a"),
            ToolExecutionPlan::read("file:/a"),
        ];
        assert_eq!(DependencyAnalyzer::analyze_plans(&conflict).len(), 2);
    }

    #[test]
    fn conservative_default_plans_never_race() {
        let plans = vec![
            ToolExecutionPlan::exclusive(),
            ToolExecutionPlan::exclusive(),
        ];
        assert_eq!(DependencyAnalyzer::analyze_plans(&plans).len(), 2);
    }
}
