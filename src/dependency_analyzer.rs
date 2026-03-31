// ===========================================================================
// Dependency Analyzer — dependency-aware tool call grouping.
//
// Analyzes a batch of tool calls to detect resource conflicts and
// dependencies, then groups them into execution phases (parallel or
// sequential) to ensure correct ordering.
// ===========================================================================

use crate::agent::stream_handler::ToolCall;

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
    "git add", "git commit", "git push", "git checkout",
    "git reset", "git merge", "git rebase", "git pull",
    "git stash", "git rm", "git mv", "git tag",
];

/// Git subcommands that only read repository state.
const GIT_READ_COMMANDS: &[&str] = &[
    "git status", "git log", "git diff", "git show",
    "git branch", "git remote",
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

// ---------------------------------------------------------------------------
// DependencyAnalyzer
// ---------------------------------------------------------------------------

/// Analyzes tool call dependencies and groups them into execution phases.
pub struct DependencyAnalyzer;

impl DependencyAnalyzer {
    /// Analyze a batch of tool calls and return execution phases.
    ///
    /// The returned phases should be executed in order.  Calls within a
    /// `Parallel` phase can run concurrently; calls within a `Sequential`
    /// phase must run one after another.
    pub fn analyze(calls: &[ToolCall]) -> Vec<ExecutionPhase> {
        if calls.is_empty() {
            return Vec::new();
        }

        // Step 1: Extract resource accesses for each call.
        let accesses: Vec<Vec<ResourceAccess>> = calls
            .iter()
            .map(|call| extract_resources(call))
            .collect();

        // Step 2: Build dependency edges.
        // depends_on[i] contains indices of calls that call i depends on.
        let n = calls.len();
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
                depth[i] = depends_on[i].iter().map(|&j| depth[j] + 1).max().unwrap_or(0);
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
            let has_conflicts = has_intra_layer_conflicts(&indices, &accesses);

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
}

// ---------------------------------------------------------------------------
// Resource extraction
// ---------------------------------------------------------------------------

fn extract_resources(call: &ToolCall) -> Vec<ResourceAccess> {
    let mut resources = Vec::new();

    match call.name.as_str() {
        "file_read" => {
            if let Some(path) = call.input.get("path").and_then(|v| v.as_str()) {
                resources.push(ResourceAccess {
                    resource: Resource::File(path.to_string()),
                    kind: AccessKind::Read,
                });
            }
        }
        "file_write" => {
            if let Some(path) = call.input.get("path").and_then(|v| v.as_str()) {
                resources.push(ResourceAccess {
                    resource: Resource::File(path.to_string()),
                    kind: AccessKind::Write,
                });
            }
        }
        "bash" => {
            if let Some(command) = call.input.get("command").and_then(|v| v.as_str()) {
                if let Some(kind) = classify_git_command(command) {
                    resources.push(ResourceAccess {
                        resource: Resource::Git,
                        kind,
                    });
                }
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
    for e in earlier {
        for l in later {
            if e.resource == l.resource {
                // Any combination where at least one is a write creates a dependency.
                if e.kind == AccessKind::Write || l.kind == AccessKind::Write {
                    return true;
                }
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
        let calls = vec![
            ToolCall::new("bash", json!({"command": "ls"})),
            ToolCall::new("bash", json!({"command": "cat README"})),
        ];
        let phases = DependencyAnalyzer::analyze(&calls);
        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], ExecutionPhase::Parallel(_)));
    }

    #[test]
    fn detects_write_then_read_dependency() {
        let calls = vec![
            ToolCall::new("file_write", json!({"path": "f.txt"})),
            ToolCall::new("file_read", json!({"path": "f.txt"})),
        ];
        assert!(DependencyAnalyzer::analyze(&calls).len() >= 2);
    }

    #[test]
    fn detects_same_resource_conflict() {
        let calls = vec![
            ToolCall::new("file_write", json!({"path": "x.txt"})),
            ToolCall::new("file_write", json!({"path": "x.txt"})),
        ];
        let phases = DependencyAnalyzer::analyze(&calls);
        assert!(phases
            .iter()
            .any(|p| matches!(p, ExecutionPhase::Sequential(_))));
    }

    #[test]
    fn groups_mixed_calls() {
        let calls = vec![
            ToolCall::new("bash", json!({"command": "echo A"})),
            ToolCall::new("bash", json!({"command": "echo B"})),
            ToolCall::new("file_read", json!({"path": "unrelated.txt"})),
        ];
        assert!(!DependencyAnalyzer::analyze(&calls).is_empty());
    }

    #[test]
    fn infers_git_side_effects() {
        let calls = vec![
            ToolCall::new("bash", json!({"command": "git add ."})),
            ToolCall::new("bash", json!({"command": "git status"})),
        ];
        assert!(DependencyAnalyzer::analyze(&calls).len() >= 2);
    }
}
