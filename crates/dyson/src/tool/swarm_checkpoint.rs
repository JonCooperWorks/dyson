// ===========================================================================
// SwarmCheckpoint tool — emit a progress checkpoint for a running task.
//
// This tool attaches a CheckpointEvent to the ToolOutput side-channel,
// mirroring how `send_file` attaches file paths.  The controller's
// `Output::checkpoint` hook decides what to do with the event:
//
//   - SwarmController  → POST /swarm/checkpoint to the hub so callers
//                        polling swarm_task_checkpoints see progress
//   - every other      → default no-op; the tool is harmless here
//
// This means the tool can be registered unconditionally in BuiltinSkill
// and doesn't need any per-controller wiring.  If a terminal/telegram
// agent happens to call it, the checkpoint is silently dropped and the
// tool still returns a successful "checkpoint recorded" message.
//
// The LLM sees a normal tool result echoing the message, which gives it
// confidence the checkpoint was accepted and lets it continue to the
// next step.
// ===========================================================================

use async_trait::async_trait;

use crate::error::Result;
use crate::tool::{CheckpointEvent, Tool, ToolContext, ToolOutput};

pub struct SwarmCheckpointTool;

#[async_trait]
impl Tool for SwarmCheckpointTool {
    fn name(&self) -> &str {
        "swarm_checkpoint"
    }

    fn description(&self) -> &str {
        "Emit a progress checkpoint for a long-running swarm task. Use this \
         periodically while executing a task dispatched via swarm_submit so \
         the caller can observe progress without waiting for the final \
         result — for example once per epoch during model fine-tuning, once \
         per batch during data processing, or once per logical step during \
         a multi-stage job. The `message` is a short human-readable note \
         (e.g. \"epoch 3/10 complete\"), and `progress` is an optional \
         fractional value between 0.0 and 1.0. Outside of a swarm task \
         this tool is a harmless no-op."
    }

    fn agent_only(&self) -> bool {
        // Emits a controller-side side-channel event; not something the
        // provider's own tool catalog can satisfy.
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Human-readable progress note (e.g. \"epoch 3/10\", \"batch 42 done\")."
                },
                "progress": {
                    "type": "number",
                    "description": "Optional fractional progress in the range 0.0..=1.0.",
                    "minimum": 0.0,
                    "maximum": 1.0
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }

    async fn run(
        &self,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let message = match input.get("message").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "swarm_checkpoint: 'message' is required",
                ));
            }
        };

        let progress = input.get("progress").and_then(|v| v.as_f64()).map(|f| {
            // Clamp to the documented range so a misbehaving call can't
            // produce nonsense values downstream.
            (f.clamp(0.0, 1.0)) as f32
        });

        let reply = match progress {
            Some(p) => format!("checkpoint recorded: {message} ({:.0}%)", p * 100.0),
            None => format!("checkpoint recorded: {message}"),
        };

        Ok(ToolOutput::success(reply).with_checkpoint(CheckpointEvent {
            message,
            progress,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn emits_checkpoint_with_message_and_progress() {
        let tool = SwarmCheckpointTool;
        let input = serde_json::json!({
            "message": "epoch 3/10",
            "progress": 0.3
        });
        let tmp = tempfile::tempdir().unwrap();
        let out = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert_eq!(out.checkpoints.len(), 1);
        assert_eq!(out.checkpoints[0].message, "epoch 3/10");
        assert_eq!(out.checkpoints[0].progress, Some(0.3));
        assert!(out.content.contains("checkpoint recorded: epoch 3/10"));
    }

    #[tokio::test]
    async fn emits_checkpoint_without_progress() {
        let tool = SwarmCheckpointTool;
        let input = serde_json::json!({ "message": "started" });
        let tmp = tempfile::tempdir().unwrap();
        let out = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert_eq!(out.checkpoints.len(), 1);
        assert!(out.checkpoints[0].progress.is_none());
    }

    #[tokio::test]
    async fn missing_message_is_error() {
        let tool = SwarmCheckpointTool;
        let input = serde_json::json!({});
        let tmp = tempfile::tempdir().unwrap();
        let out = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.checkpoints.is_empty());
    }

    #[tokio::test]
    async fn progress_is_clamped() {
        let tool = SwarmCheckpointTool;
        let tmp = tempfile::tempdir().unwrap();

        let high = tool
            .run(
                &serde_json::json!({"message": "ok", "progress": 42.0}),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(high.checkpoints[0].progress, Some(1.0));

        let low = tool
            .run(
                &serde_json::json!({"message": "ok", "progress": -1.0}),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert_eq!(low.checkpoints[0].progress, Some(0.0));
    }
}
