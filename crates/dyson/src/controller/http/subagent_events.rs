// ===========================================================================
// SubagentEventBus — HTTP UI side-channel for nested subagent tool calls.
//
// A subagent's `CaptureOutput` (see `crate::skill::subagent::mod`) swallows
// the inner agent's tool calls so the parent's LLM conversation stays clean —
// dragging every nested `tool_use` into the parent's context window would
// blow the token budget after a couple of subagent invocations.
//
// But the HTTP frontend wants to render those nested calls live, inside the
// subagent's tool box, so the user can watch what a security_engineer or
// coder run is actually doing instead of staring at an empty panel for
// minutes.  This module is the second event path the comment on
// `CaptureOutput` describes: a side-channel that bypasses the LLM boundary
// entirely and reaches the chat's broadcast channel directly.
//
// Threading: `routes::turns` builds one of these per turn from the
// `ChatHandle.events` sender, hands it to `Agent::set_subagent_events`,
// the agent drops it into `ToolContext.subagent_events`, and the
// subagent tools in `crate::skill::subagent` forward it into `ChildSpawn`
// so `CaptureOutput` can tee tagged events through it.
//
// Send is best-effort: when there are no subscribers (the user closed the
// browser tab) the broadcast send returns an error which we swallow.  The
// rolling replay ring on `ChatHandle` is the authoritative recovery path
// for a reconnect, and these tee-events do not push into that ring —
// they're live-only.  A reconnect mid-subagent shows the parent panel as
// empty until the next nested event arrives, which matches the existing
// behavior for any other live SSE state on reconnect.
// ===========================================================================

use tokio::sync::broadcast;

use crate::tool::ToolOutput;

use super::wire::SseEvent;

/// Tee channel for nested subagent UI events.  Cheap to clone — wraps a
/// `broadcast::Sender` (which is itself `Arc`-internally).
#[derive(Clone)]
pub struct SubagentEventBus {
    tx: broadcast::Sender<SseEvent>,
}

impl SubagentEventBus {
    pub(crate) fn new(tx: broadcast::Sender<SseEvent>) -> Self {
        Self { tx }
    }

    /// Emit an inner `tool_use_start` tagged with the parent subagent
    /// tool's id.  Call this from inside a `CaptureOutput` so the
    /// frontend can attach a child chip to the right subagent panel.
    pub fn tool_start(&self, parent_tool_id: &str, child_id: &str, name: &str) {
        let _ = self.tx.send(SseEvent::ToolStart {
            id: child_id.to_string(),
            name: name.to_string(),
            parent_tool_id: Some(parent_tool_id.to_string()),
        });
    }

    /// Emit an inner `tool_result`.  `child_id` is the tool_use_id of
    /// the call that just finished — the frontend uses it to update the
    /// correct child even when the subagent dispatched calls in
    /// parallel (where the most-recent-tool-start heuristic is unsafe).
    pub fn tool_result(
        &self,
        parent_tool_id: &str,
        child_id: Option<&str>,
        output: &ToolOutput,
    ) {
        let _ = self.tx.send(SseEvent::ToolResult {
            content: output.content.clone(),
            is_error: output.is_error,
            view: output.view.clone(),
            parent_tool_id: Some(parent_tool_id.to_string()),
            tool_use_id: child_id.map(str::to_string),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::view::ToolView;

    fn fixture() -> (SubagentEventBus, broadcast::Receiver<SseEvent>) {
        let (tx, rx) = broadcast::channel(64);
        (SubagentEventBus::new(tx), rx)
    }

    #[test]
    fn tool_start_carries_parent_id() {
        let (bus, mut rx) = fixture();
        bus.tool_start("parent_42", "child_7", "bash");
        match rx.try_recv().unwrap() {
            SseEvent::ToolStart { id, name, parent_tool_id } => {
                assert_eq!(id, "child_7");
                assert_eq!(name, "bash");
                assert_eq!(parent_tool_id.as_deref(), Some("parent_42"));
            }
            other => panic!("unexpected event: {}", serde_json::to_string(&other).unwrap()),
        }
    }

    #[test]
    fn tool_result_carries_parent_and_child_id_and_view() {
        let (bus, mut rx) = fixture();
        let mut out = ToolOutput::success("hello");
        out.view = Some(ToolView::Bash {
            lines: vec![],
            exit_code: Some(0),
            duration_ms: 12,
        });
        bus.tool_result("parent_42", Some("child_7"), &out);
        match rx.try_recv().unwrap() {
            SseEvent::ToolResult {
                content,
                is_error,
                view,
                parent_tool_id,
                tool_use_id,
            } => {
                assert_eq!(content, "hello");
                assert!(!is_error);
                assert!(view.is_some());
                assert_eq!(parent_tool_id.as_deref(), Some("parent_42"));
                assert_eq!(tool_use_id.as_deref(), Some("child_7"));
            }
            other => panic!("unexpected event: {}", serde_json::to_string(&other).unwrap()),
        }
    }

    #[test]
    fn send_with_no_subscribers_is_swallowed() {
        // Channel exists but no receiver — best-effort send must not
        // bubble up an error.  Drop the receiver explicitly to be sure.
        let (tx, rx) = broadcast::channel::<SseEvent>(8);
        drop(rx);
        let bus = SubagentEventBus::new(tx);
        bus.tool_start("p", "c", "bash");
    }
}
