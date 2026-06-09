//! The SecurityHarnessRuntime bundle + the small wrappers that spawn one child
//! per stage and merge its ToolOutput back into the running aggregate.
//!
//! The runtime is constructed by the orchestrator after the parent-side
//! resolution of provider/model/sandbox/etc., then handed to
//! `run_security_harness`.  Stage runners use it read-only.

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::llm::LlmClient;
use crate::message::ArtefactKind;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolOutput};
use crate::workspace::WorkspaceHandle;

use super::types::{SecurityCheckpoint, SecurityHarnessStage};
use crate::skill::subagent::orchestrator::OrchestratorInput;
use crate::skill::subagent::{ChildSpawn, spawn_child};

pub(crate) struct SecurityHarnessRuntime {
    pub config_name: &'static str,
    pub provider: LlmProvider,
    pub model: String,
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Option<WorkspaceHandle>,
    pub parent_depth: u8,
    pub scoped_dir: Option<PathBuf>,
    pub parent_working_dir: PathBuf,
    pub all_tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub user_message: String,
    pub parsed: OrchestratorInput,
    pub activity: Option<crate::controller::ActivityHandle>,
    pub events: Option<crate::controller::http::SubagentEventBus>,
    pub parent_tool_id: Option<String>,
    pub emit_artefact: Option<ArtefactKind>,
    pub max_tokens: u32,
}

pub(super) async fn spawn_stage(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    checkpoint: &SecurityCheckpoint,
    max_iterations: usize,
) -> std::result::Result<(String, ToolOutput), String> {
    let checkpoint_json = serde_json::to_string_pretty(checkpoint)
        .map_err(|e| format!("serialize checkpoint for {stage}: {e}"))?;
    let system_prompt = format!("{}\n\n{}", rt.system_prompt, prompt);
    let stage_message = format!(
        "Parent request:\n{}\n\nCurrent durable checkpoint JSON:\n```json\n{}\n```\n",
        rt.user_message, checkpoint_json
    );
    let settings = AgentSettings::for_child(
        rt.model.clone(),
        rt.provider.clone(),
        max_iterations,
        rt.max_tokens,
        system_prompt,
    );
    let out = spawn_child(ChildSpawn {
        name: stage.as_str(),
        settings,
        inherited_tools: rt.all_tools.clone(),
        sandbox: Arc::clone(&rt.sandbox),
        workspace: rt.workspace.clone(),
        client: rt.client.clone(),
        parent_depth: rt.parent_depth,
        working_dir: rt.scoped_dir.clone(),
        user_message: stage_message,
        activity: rt.activity.clone(),
        events: rt.events.clone(),
        parent_tool_id: rt.parent_tool_id.clone(),
    })
    .await
    .map_err(|e| e.to_string())?;
    if out.is_error {
        return Err(out.content);
    }
    Ok((out.content.clone(), out))
}

pub(super) fn merge_stage_tool_output(target: &mut ToolOutput, mut stage: ToolOutput) {
    target.checkpoints.append(&mut stage.checkpoints);
    target.artefacts.append(&mut stage.artefacts);
    let Some(stage_meta) = stage.metadata.take() else {
        return;
    };
    let mut meta = target.metadata.take().unwrap_or_else(|| {
        serde_json::json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "llm_calls": 0,
        })
    });
    for key in ["input_tokens", "output_tokens", "llm_calls"] {
        let current = meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let add = stage_meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        meta[key] = serde_json::json!(current + add);
    }
    target.metadata = Some(meta);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Artefact, ArtefactKind};
    use crate::tool::CheckpointEvent;

    fn meta(input: u64, output: u64, calls: u64) -> serde_json::Value {
        serde_json::json!({
            "input_tokens": input,
            "output_tokens": output,
            "llm_calls": calls,
        })
    }

    #[test]
    fn merge_sums_token_and_call_counters_into_target() {
        let mut target = ToolOutput::success(String::new());
        target.metadata = Some(meta(10, 20, 3));
        let mut stage = ToolOutput::success(String::new());
        stage.metadata = Some(meta(5, 7, 2));
        merge_stage_tool_output(&mut target, stage);
        let m = target.metadata.expect("metadata");
        assert_eq!(
            m["input_tokens"].as_u64().unwrap(),
            15,
            "input_tokens should sum target+stage"
        );
        assert_eq!(
            m["output_tokens"].as_u64().unwrap(),
            27,
            "output_tokens should sum target+stage"
        );
        assert_eq!(
            m["llm_calls"].as_u64().unwrap(),
            5,
            "llm_calls should sum target+stage"
        );
    }

    #[test]
    fn merge_appends_artefacts_from_stage_into_target() {
        let mut target = ToolOutput::success(String::new());
        target.artefacts.push(Artefact::markdown(
            ArtefactKind::SecurityReview,
            "T",
            "target content",
        ));
        let mut stage = ToolOutput::success(String::new());
        stage.artefacts.push(Artefact::markdown(
            ArtefactKind::SecurityReview,
            "S",
            "stage content",
        ));
        merge_stage_tool_output(&mut target, stage);
        assert_eq!(
            target.artefacts.len(),
            2,
            "merge should append stage artefacts into target"
        );
        assert_eq!(target.artefacts[0].title, "T");
        assert_eq!(target.artefacts[1].title, "S");
    }

    #[test]
    fn merge_appends_checkpoints_from_stage_into_target() {
        let mut target = ToolOutput::success(String::new());
        target.checkpoints.push(CheckpointEvent {
            message: "target".into(),
            progress: Some(0.1),
        });
        let mut stage = ToolOutput::success(String::new());
        stage.checkpoints.push(CheckpointEvent {
            message: "stage-1".into(),
            progress: Some(0.2),
        });
        stage.checkpoints.push(CheckpointEvent {
            message: "stage-2".into(),
            progress: Some(0.3),
        });
        merge_stage_tool_output(&mut target, stage);
        assert_eq!(
            target.checkpoints.len(),
            3,
            "merge should append every stage checkpoint into target"
        );
        assert_eq!(target.checkpoints[1].message, "stage-1");
        assert_eq!(target.checkpoints[2].message, "stage-2");
    }

    #[test]
    fn merge_initializes_target_metadata_when_target_had_none() {
        let mut target = ToolOutput::success(String::new());
        assert!(
            target.metadata.is_none(),
            "precondition: no target metadata"
        );
        let mut stage = ToolOutput::success(String::new());
        stage.metadata = Some(meta(4, 8, 1));
        merge_stage_tool_output(&mut target, stage);
        let m = target.metadata.expect("metadata should be initialized");
        assert_eq!(m["input_tokens"].as_u64().unwrap(), 4);
        assert_eq!(m["output_tokens"].as_u64().unwrap(), 8);
        assert_eq!(m["llm_calls"].as_u64().unwrap(), 1);
    }

    #[test]
    fn merge_leaves_target_metadata_unchanged_when_stage_has_none() {
        let mut target = ToolOutput::success(String::new());
        target.metadata = Some(meta(11, 22, 3));
        let stage = ToolOutput::success(String::new());
        merge_stage_tool_output(&mut target, stage);
        let m = target.metadata.expect("metadata should remain");
        assert_eq!(
            m["input_tokens"].as_u64().unwrap(),
            11,
            "merge with empty stage metadata should not mutate target counters"
        );
        assert_eq!(m["output_tokens"].as_u64().unwrap(), 22);
        assert_eq!(m["llm_calls"].as_u64().unwrap(), 3);
    }
}
