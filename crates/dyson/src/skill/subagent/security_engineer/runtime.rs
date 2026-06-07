// ===========================================================================
// The SecurityHarnessRuntime bundle + the small wrappers that spawn one child
// per stage and merge its ToolOutput back into the running aggregate.
//
// The runtime is constructed by the orchestrator after the parent-side
// resolution of provider/model/sandbox/etc., then handed to
// `run_security_harness`.  Stage runners use it read-only.
// ===========================================================================

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
    spawn_stage_with_checkpoint(rt, stage, prompt, checkpoint, max_iterations).await
}

pub(super) async fn spawn_stage_with_checkpoint(
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
    let settings = AgentSettings {
        model: rt.model.clone(),
        max_iterations,
        max_tokens: rt.max_tokens,
        system_prompt,
        provider: rt.provider.clone(),
        ..AgentSettings::default()
    };
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
