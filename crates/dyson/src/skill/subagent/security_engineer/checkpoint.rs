// ===========================================================================
// Durable checkpoint persistence + small helpers for run identity, target
// metadata, and stop-after-stage routing.
//
// CheckpointStore writes JSON under either the Dyson workspace (preferred —
// Swarm sync mirrors it) or a local fallback under `.dyson/security-harness/`.
// ===========================================================================

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
