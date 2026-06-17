//! Durable checkpoint persistence + small helpers for run identity, target
//! metadata, and stop-after-stage routing.
//!
//! CheckpointStore writes JSON under either the Dyson workspace (preferred —
//! Swarm sync mirrors it) or a local fallback under `.dyson/security-harness/`.

use std::path::{Path, PathBuf};

use crate::config::LlmProvider;
use crate::workspace::WorkspaceHandle;

use super::SECURITY_HARNESS_SCHEMA_VERSION;
use super::types::{SecurityCheckpoint, SecurityHarnessStage};
use crate::skill::subagent::orchestrator::OrchestratorInput;

const CHECKPOINT_PREFIX: &str = "kb/security-harness/checkpoints";

pub(super) struct CheckpointStore {
    workspace: Option<WorkspaceHandle>,
    fallback_dir: PathBuf,
}

impl CheckpointStore {
    pub(super) fn new(workspace: Option<WorkspaceHandle>, working_dir: PathBuf) -> Self {
        Self {
            workspace,
            fallback_dir: working_dir
                .join(".dyson")
                .join("security-harness")
                .join("checkpoints"),
        }
    }

    pub(super) async fn save(
        &self,
        checkpoint: &SecurityCheckpoint,
    ) -> std::result::Result<(), String> {
        let body = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
        if let Some(workspace) = &self.workspace {
            let mut guard = workspace.write().await;
            guard.set(&checkpoint.checkpoint_path(), &body);
            guard.save().map_err(|e| e.to_string())?;
            return Ok(());
        }
        std::fs::create_dir_all(&self.fallback_dir).map_err(|e| {
            format!(
                "cannot create checkpoint dir {}: {e}",
                self.fallback_dir.display()
            )
        })?;
        std::fs::write(
            self.fallback_dir
                .join(format!("{}.json", checkpoint.run_id)),
            body,
        )
        .map_err(|e| format!("cannot write checkpoint: {e}"))
    }

    pub(super) async fn load_exact(
        &self,
        run_id: &str,
    ) -> std::result::Result<SecurityCheckpoint, String> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let path = checkpoint_path(run_id);
            let Some(body) = guard.get(&path) else {
                let disk_root = guard
                    .programs_dir()
                    .and_then(|programs| programs.parent().map(Path::to_path_buf));
                drop(guard);
                if let Some(root) = disk_root {
                    let path = root.join(checkpoint_path(run_id));
                    let body = std::fs::read_to_string(&path).map_err(|_| {
                        format!("checkpoint {run_id} not found at {}", path.display())
                    })?;
                    return parse_checkpoint(&body);
                }
                return Err(format!("checkpoint {run_id} not found"));
            };
            return parse_checkpoint(&body);
        }
        let path = self.fallback_dir.join(format!("{run_id}.json"));
        let body = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read checkpoint {}: {e}", path.display()))?;
        parse_checkpoint(&body)
    }

    pub(super) async fn list(&self) -> Vec<SecurityCheckpoint> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let mut checkpoints: Vec<SecurityCheckpoint> = guard
                .list_files()
                .into_iter()
                .filter(|p| p.starts_with(CHECKPOINT_PREFIX) && p.ends_with(".json"))
                .filter_map(|p| guard.get(&p).and_then(|body| parse_checkpoint(&body).ok()))
                .collect();
            let disk_root = guard
                .programs_dir()
                .and_then(|programs| programs.parent().map(Path::to_path_buf));
            drop(guard);
            if let Some(root) = disk_root {
                checkpoints.extend(read_checkpoint_dir(root.join(CHECKPOINT_PREFIX)));
                checkpoints.sort_by(|a, b| a.run_id.cmp(&b.run_id));
                checkpoints.dedup_by(|a, b| a.run_id == b.run_id);
            }
            return checkpoints;
        }
        read_checkpoint_dir(self.fallback_dir.clone())
    }
}

pub(super) fn read_checkpoint_dir(path: PathBuf) -> Vec<SecurityCheckpoint> {
    let Ok(entries) = std::fs::read_dir(&path) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let body = std::fs::read_to_string(path).ok()?;
            parse_checkpoint(&body).ok()
        })
        .collect()
}

pub(super) fn parse_checkpoint(body: &str) -> std::result::Result<SecurityCheckpoint, String> {
    let checkpoint: SecurityCheckpoint = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if checkpoint.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported checkpoint schema_version {}; expected {}",
            checkpoint.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        ));
    }
    if checkpoint.harness_version != super::SECURITY_HARNESS_VERSION {
        return Err(format!(
            "unsupported checkpoint harness_version {}; expected {}",
            checkpoint.harness_version,
            super::SECURITY_HARNESS_VERSION
        ));
    }
    Ok(checkpoint)
}

pub(super) async fn load_checkpoint_for_resume(
    store: &CheckpointStore,
    run_id: Option<&str>,
    target_path: &str,
) -> std::result::Result<SecurityCheckpoint, String> {
    if let Some(run_id) = run_id.filter(|s| !s.trim().is_empty()) {
        return store.load_exact(run_id.trim()).await;
    }
    let mut matches: Vec<SecurityCheckpoint> = store
        .list()
        .await
        .into_iter()
        .filter(|cp| !cp.completed && cp.target.repo_path == target_path)
        .collect();
    matches.sort_by_key(|cp| std::cmp::Reverse(cp.updated_at));
    match matches.len() {
        0 => Err(format!(
            "no incomplete security_engineer checkpoint found for {target_path}"
        )),
        1 => Ok(matches.remove(0)),
        _ => {
            let list = matches
                .iter()
                .take(8)
                .map(|cp| {
                    format!(
                        "- {} stage={} updated_at={}",
                        cp.run_id, cp.current_stage, cp.updated_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "multiple incomplete security_engineer checkpoints found; rerun with run_id:\n{list}"
            ))
        }
    }
}

pub(super) fn checkpoint_path(run_id: &str) -> String {
    format!("{CHECKPOINT_PREFIX}/{run_id}.json")
}

pub(super) fn make_run_id() -> String {
    format!(
        "sec-{}-{}",
        unix_seconds(std::time::SystemTime::now()),
        std::process::id()
    )
}

pub(super) fn unix_seconds(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn provider_label(provider: &LlmProvider) -> String {
    format!("{provider:?}")
}

pub(super) fn target_name_for(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("target")
        .to_string()
}

pub(super) fn git_ref_for(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn scope_for(parsed: &OrchestratorInput) -> String {
    let mut parts = Vec::new();
    if !parsed.path.trim().is_empty() {
        parts.push(format!("path={}", parsed.path.trim()));
    }
    if !parsed.context.trim().is_empty() {
        parts.push(format!("context={}", parsed.context.trim()));
    }
    parts.push(format!("task={}", parsed.task.trim()));
    parts.join("\n")
}

pub(super) fn should_stop_after(parsed: &OrchestratorInput, stage: SecurityHarnessStage) -> bool {
    parsed
        .stop_after_stage
        .as_deref()
        .and_then(SecurityHarnessStage::parse)
        == Some(stage)
}

#[cfg(test)]
mod tests {
    use super::super::types::{ModelMetadata, SecurityHarnessStage, TargetRef};
    use super::*;
    use crate::config::LlmProvider;
    use crate::workspace::InMemoryWorkspace;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn parsed_with_stop(stop: Option<&str>) -> OrchestratorInput {
        OrchestratorInput {
            task: String::new(),
            context: String::new(),
            path: String::new(),
            resume: false,
            run_id: None,
            stop_after_stage: stop.map(str::to_string),
        }
    }

    fn fresh_checkpoint(run_id: &str, target_path: &str, updated_at: u64) -> SecurityCheckpoint {
        let mut cp = SecurityCheckpoint::new(
            run_id.into(),
            TargetRef {
                repo_path: target_path.into(),
                git_ref: None,
            },
            "scope".into(),
            ModelMetadata {
                provider: "p".into(),
                model: "m".into(),
            },
            0,
        );
        cp.updated_at = updated_at;
        cp
    }

    fn make_workspace() -> crate::workspace::WorkspaceHandle {
        Arc::new(tokio::sync::RwLock::new(
            Box::new(InMemoryWorkspace::new()) as Box<dyn crate::workspace::Workspace>
        ))
    }

    #[test]
    fn make_run_id_remains_stable_within_a_second_for_the_same_process() {
        // Pin actual behavior: make_run_id is `sec-<unix_seconds>-<pid>`,
        // NOT a counter — so within the same second from the same process
        // it repeats. Resume disambiguation relies on the workspace path
        // (one save = one file) rather than a per-call counter, plus the
        // caller's own time advancing. If we ever switch to a counter,
        // update this test (and the path is no longer load-bearing).
        let ids: HashSet<String> = (0..32).map(|_| make_run_id()).collect();
        assert!(
            !ids.is_empty() && ids.len() <= 2,
            "make_run_id is time+pid based; 32 same-second calls collapse to 1-2 ids, got {}",
            ids.len()
        );
    }

    #[test]
    fn make_run_id_matches_sec_unix_pid_shape() {
        let id = make_run_id();
        assert!(id.starts_with("sec-"), "run id should start with sec-");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "expected sec-<unix>-<pid>, got {id}");
        assert_eq!(parts[0], "sec");
        assert!(
            parts[1].chars().all(|c| c.is_ascii_digit()),
            "unix portion should be digits, got {}",
            parts[1]
        );
        assert!(
            parts[2].chars().all(|c| c.is_ascii_digit()),
            "pid portion should be digits, got {}",
            parts[2]
        );
    }

    #[test]
    fn checkpoint_path_puts_run_id_under_prefix_with_json_suffix() {
        let path = checkpoint_path("sec-1-2");
        assert!(
            path.starts_with(CHECKPOINT_PREFIX),
            "path should be under CHECKPOINT_PREFIX, got {path}"
        );
        assert!(
            path.ends_with(".json"),
            "path should end with .json, got {path}"
        );
        assert_eq!(path, "kb/security-harness/checkpoints/sec-1-2.json");
    }

    #[test]
    fn git_ref_for_returns_none_for_fresh_non_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            git_ref_for(dir.path()),
            None,
            "git_ref_for should return None for a non-git directory"
        );
    }

    #[test]
    fn target_name_for_empty_returns_fallback_placeholder() {
        // Pin the current behavior: an empty path can't yield a file name,
        // so Path::file_name returns None and we fall back to "target".
        assert_eq!(
            target_name_for(""),
            "target",
            "empty path falls back to literal target name"
        );
    }

    #[test]
    fn target_name_for_full_path_returns_last_segment() {
        assert_eq!(target_name_for("/foo/bar/baz"), "baz");
    }

    #[test]
    fn target_name_for_trailing_slash_returns_last_segment() {
        assert_eq!(target_name_for("/foo/bar/baz/"), "baz");
    }

    #[test]
    fn target_name_for_single_segment_returns_input() {
        assert_eq!(target_name_for("single"), "single");
    }

    #[test]
    fn provider_label_is_stable_for_every_variant() {
        // The label is just the Debug representation today; if that ever
        // changes, this test pins the contract so downstream consumers
        // (artefact metadata, checkpoints, telemetry) update with us.
        assert_eq!(provider_label(&LlmProvider::Anthropic), "Anthropic");
        assert_eq!(provider_label(&LlmProvider::OpenAi), "OpenAi");
        assert_eq!(provider_label(&LlmProvider::OpenRouter), "OpenRouter");
        assert_eq!(provider_label(&LlmProvider::ClaudeCode), "ClaudeCode");
        assert_eq!(provider_label(&LlmProvider::Codex), "Codex");
        assert_eq!(provider_label(&LlmProvider::OllamaCloud), "OllamaCloud");
        assert_eq!(provider_label(&LlmProvider::Gemini), "Gemini");
    }

    #[test]
    fn scope_for_formats_path_context_task_block() {
        let parsed = OrchestratorInput {
            task: " review auth ".into(),
            context: " mcp runtime ".into(),
            path: " /repo/src ".into(),
            resume: false,
            run_id: None,
            stop_after_stage: None,
        };
        let scope = scope_for(&parsed);
        assert_eq!(
            scope, "path=/repo/src\ncontext=mcp runtime\ntask=review auth",
            "scope_for should trim each field and format path/context/task lines"
        );
    }

    #[test]
    fn scope_for_omits_empty_path_and_context() {
        let parsed = OrchestratorInput {
            task: "review".into(),
            context: String::new(),
            path: String::new(),
            resume: false,
            run_id: None,
            stop_after_stage: None,
        };
        assert_eq!(
            scope_for(&parsed),
            "task=review",
            "empty path/context should be omitted entirely"
        );
    }

    #[test]
    fn should_stop_after_matches_stage_string() {
        let parsed = parsed_with_stop(Some("hunt"));
        assert!(
            should_stop_after(&parsed, SecurityHarnessStage::Hunt),
            "stop_after_stage=hunt should match SecurityHarnessStage::Hunt"
        );
        assert!(
            !should_stop_after(&parsed, SecurityHarnessStage::Recon),
            "stop_after_stage=hunt should NOT match SecurityHarnessStage::Recon"
        );
    }

    #[test]
    fn should_stop_after_returns_false_when_none() {
        let parsed = parsed_with_stop(None);
        for stage in [
            SecurityHarnessStage::Recon,
            SecurityHarnessStage::Hunt,
            SecurityHarnessStage::Validate,
            SecurityHarnessStage::Gapfill,
            SecurityHarnessStage::Dedupe,
            SecurityHarnessStage::Trace,
            SecurityHarnessStage::Feedback,
            SecurityHarnessStage::Report,
        ] {
            assert!(
                !should_stop_after(&parsed, stage),
                "stop_after_stage=None should not stop at {stage}"
            );
        }
    }

    #[tokio::test]
    async fn checkpoint_store_save_then_load_exact_round_trip() {
        let workspace = make_workspace();
        let store = CheckpointStore::new(Some(workspace.clone()), PathBuf::from("/tmp"));
        let mut cp = fresh_checkpoint("sec-rt-1", "/repo", 100);
        cp.architecture_context = "context".into();
        cp.pending_tasks.push(super::super::types::SecurityTask {
            id: "t1".into(),
            attack_class: "auth_authorization".into(),
            scope_hint: "scope".into(),
            status: super::super::types::TaskStatus::Pending,
            rationale: "r".into(),
        });
        store.save(&cp).await.expect("save");
        let loaded = store.load_exact("sec-rt-1").await.expect("load");
        assert_eq!(
            cp, loaded,
            "save+load_exact should round-trip the checkpoint"
        );
    }

    #[tokio::test]
    async fn checkpoint_store_list_returns_multiple_saved_checkpoints() {
        let workspace = make_workspace();
        let store = CheckpointStore::new(Some(workspace.clone()), PathBuf::from("/tmp"));
        for (run_id, updated_at) in [
            ("sec-list-1", 100u64),
            ("sec-list-2", 200),
            ("sec-list-3", 300),
        ] {
            let cp = fresh_checkpoint(run_id, "/repo", updated_at);
            store.save(&cp).await.expect("save");
        }
        let listed = store.list().await;
        // The store's list() sorts ascending by run_id for in-memory
        // dedupe with the disk overlay; downstream
        // load_checkpoint_for_resume re-sorts by updated_at descending.
        assert_eq!(
            listed.len(),
            3,
            "list should return every saved checkpoint, got {}",
            listed.len()
        );
        let ids: HashSet<_> = listed.iter().map(|cp| cp.run_id.clone()).collect();
        for expected in ["sec-list-1", "sec-list-2", "sec-list-3"] {
            assert!(ids.contains(expected), "missing {expected} in list");
        }
    }

    #[tokio::test]
    async fn load_for_resume_with_run_id_ignores_target_path_filter() {
        let workspace = make_workspace();
        let store = CheckpointStore::new(Some(workspace.clone()), PathBuf::from("/tmp"));
        let cp = fresh_checkpoint("sec-resume-id", "/other/path", 100);
        store.save(&cp).await.expect("save");
        let loaded = load_checkpoint_for_resume(&store, Some("sec-resume-id"), "/unrelated")
            .await
            .expect("load_checkpoint_for_resume with run_id should succeed");
        assert_eq!(loaded.run_id, "sec-resume-id");
    }

    #[tokio::test]
    async fn load_for_resume_without_run_id_and_no_matches_errors() {
        let workspace = make_workspace();
        let store = CheckpointStore::new(Some(workspace.clone()), PathBuf::from("/tmp"));
        let err = load_checkpoint_for_resume(&store, None, "/missing")
            .await
            .expect_err("no matches should be an error");
        assert!(
            err.contains("no incomplete security_engineer checkpoint"),
            "expected 'no incomplete security_engineer checkpoint' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn load_for_resume_without_run_id_and_many_matches_lists_run_ids() {
        let workspace = make_workspace();
        let store = CheckpointStore::new(Some(workspace.clone()), PathBuf::from("/tmp"));
        for (run_id, updated_at) in [("sec-multi-1", 100u64), ("sec-multi-2", 200)] {
            let cp = fresh_checkpoint(run_id, "/repo", updated_at);
            store.save(&cp).await.expect("save");
        }
        let err = load_checkpoint_for_resume(&store, None, "/repo")
            .await
            .expect_err("multiple matches should be an error");
        assert!(
            err.contains("sec-multi-1"),
            "error should list sec-multi-1, got: {err}"
        );
        assert!(
            err.contains("sec-multi-2"),
            "error should list sec-multi-2, got: {err}"
        );
    }
}
