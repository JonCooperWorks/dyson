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

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::tool::ToolOutput;

use super::state::EventRing;
use super::wire::SseEvent;

/// Tee channel for nested subagent UI events.
///
/// `tx` is the broadcast sender shared with live SSE subscribers.
/// `ring` is the chat's rolling replay buffer — `checkpoint` events
/// push here so that a browser reloading mid-subagent can replay the
/// stage / run-id / findings lines it missed.  `tool_start` /
/// `tool_result` are deliberately NOT ringed (the nested child
/// chips are ephemeral UI state, and a long subagent would blow the
/// 4096-slot cap with read_file/bash chips alone — those re-stream
/// from the live broadcast only).  Cheap to clone — broadcast::Sender
/// and `Arc<Mutex<_>>` are both Arc-y inside.
#[derive(Clone)]
pub struct SubagentEventBus {
    tx: broadcast::Sender<SseEvent>,
    ring: Option<Arc<Mutex<EventRing>>>,
}

impl SubagentEventBus {
    pub(crate) fn new(tx: broadcast::Sender<SseEvent>) -> Self {
        Self { tx, ring: None }
    }

    /// Pair the bus with the chat's replay ring so `checkpoint` events
    /// survive a mid-run reload.  Production wiring (routes::turns)
    /// always calls this; the standalone constructor stays
    /// ring-less for tests that don't need replay semantics.
    pub(crate) fn with_replay_ring(mut self, ring: Arc<Mutex<EventRing>>) -> Self {
        self.ring = Some(ring);
        self
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
    pub fn tool_result(&self, parent_tool_id: &str, child_id: Option<&str>, output: &ToolOutput) {
        let _ = self.tx.send(SseEvent::ToolResult {
            content: output.content.clone(),
            is_error: output.is_error,
            view: output.view.clone(),
            parent_tool_id: Some(parent_tool_id.to_string()),
            tool_use_id: child_id.map(str::to_string),
        });
    }

    /// Stream a `checkpoint` progress line into the conversation SSE feed
    /// during a long-running tool.  Unlike `tool_start`/`tool_result`,
    /// `checkpoint` carries no tool id — the frontend appends it to the
    /// live tool's body text, which stays pinned to the parent subagent
    /// panel for the whole run.
    ///
    /// Pushes into the replay ring AND broadcasts.  The ring push is
    /// what makes mid-subagent reloads work: without it, every
    /// `security_engineer: <stage>` line emitted before the reload
    /// was permanently lost from the page state, so a refresh on a
    /// 20-minute Hunt sat on "harness initializing — (no run id yet)"
    /// for as long as the next live event took to fire.  Now the
    /// reconnect-side replay path (`/api/conversations/:id/events`)
    /// re-emits each ringed checkpoint, the frontend's `onCheckpoint`
    /// appends them to `body.text` exactly as it did the first time,
    /// and the panel rebuilds its `StageBar` / FindingsCounter /
    /// ClassGrid without depending on the post-completion snapshot.
    ///
    /// Cost: 4096-slot ring vs ~50–80 checkpoint events per harness
    /// run is well within the bound.  Post-tool, `execution.rs` also
    /// pushes the same checkpoints via `output.checkpoint(cp)` — the
    /// duplicate is harmless because the frontend parser is
    /// line-idempotent (each `security_engineer: <stage>` line just
    /// re-sets the same lastStage; per-class hunt lines write the
    /// same `classStatus` entry).
    pub fn checkpoint(&self, text: &str) {
        let evt = SseEvent::Checkpoint {
            text: text.to_string(),
        };
        if let Some(ring) = self.ring.as_ref() {
            let mut g = match ring.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.push(evt.clone());
        }
        let _ = self.tx.send(evt);
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
            SseEvent::ToolStart {
                id,
                name,
                parent_tool_id,
            } => {
                assert_eq!(id, "child_7");
                assert_eq!(name, "bash");
                assert_eq!(parent_tool_id.as_deref(), Some("parent_42"));
            }
            other => panic!(
                "unexpected event: {}",
                serde_json::to_string(&other).unwrap()
            ),
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
            other => panic!(
                "unexpected event: {}",
                serde_json::to_string(&other).unwrap()
            ),
        }
    }

    #[test]
    fn checkpoint_emits_text_event() {
        // The staged harness streams stage/run_id lines through this so the
        // SecurityHarnessPanel's StageBar advances live instead of only on
        // completion.  The text must reach the SSE feed verbatim so the
        // panel parser can pull `sec-...` / `security_engineer: <stage>`.
        let (bus, mut rx) = fixture();
        bus.checkpoint("security_engineer: created checkpoint sec-123-4");
        match rx.try_recv().unwrap() {
            SseEvent::Checkpoint { text } => {
                assert_eq!(text, "security_engineer: created checkpoint sec-123-4");
            }
            other => panic!(
                "unexpected event: {}",
                serde_json::to_string(&other).unwrap()
            ),
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

    fn ringed_fixture() -> (
        SubagentEventBus,
        broadcast::Receiver<SseEvent>,
        Arc<Mutex<EventRing>>,
    ) {
        let (tx, rx) = broadcast::channel(64);
        let ring = Arc::new(Mutex::new(EventRing::new()));
        let bus = SubagentEventBus::new(tx).with_replay_ring(Arc::clone(&ring));
        (bus, rx, ring)
    }

    #[test]
    fn checkpoint_pushes_into_replay_ring_when_paired() {
        // The 2026-06-09 mid-run rehydration regression: every harness
        // checkpoint was live-only, so a refresh during Hunt lost the
        // stage / run-id / findings lines until the next live event
        // fired.  Now the bus also pushes each checkpoint onto the
        // chat's replay ring so the SSE reconnect path replays them.
        let (bus, mut rx, ring) = ringed_fixture();
        bus.checkpoint("security_engineer: hunt");
        bus.checkpoint("security_engineer: hunt: class injection_unsafe_execution hunted (5 findings)");

        // Live broadcast still delivered every event.
        for expected in [
            "security_engineer: hunt",
            "security_engineer: hunt: class injection_unsafe_execution hunted (5 findings)",
        ] {
            match rx.try_recv().expect("live event must arrive") {
                SseEvent::Checkpoint { text } => assert_eq!(text, expected),
                other => panic!(
                    "expected checkpoint event, got {}",
                    serde_json::to_string(&other).unwrap()
                ),
            }
        }

        // Replay ring carries the same events in order so a
        // reconnecting EventSource with Last-Event-ID=0 can rebuild
        // the panel state without the post-completion bake.
        let snapshot = ring.lock().unwrap().since(0);
        assert_eq!(snapshot.len(), 2, "ring should hold both checkpoints");
        let texts: Vec<String> = snapshot
            .iter()
            .map(|(_, e)| match e {
                SseEvent::Checkpoint { text } => text.clone(),
                _ => panic!("unexpected ring entry kind"),
            })
            .collect();
        assert_eq!(texts[0], "security_engineer: hunt");
        assert!(
            texts[1].contains("hunt: class injection_unsafe_execution hunted (5 findings)"),
            "second checkpoint should be the class-hunted line"
        );
    }

    #[test]
    fn tool_start_and_tool_result_stay_live_only_even_with_replay_ring() {
        // The ring is capped at 4096 entries (state.rs).  A long
        // subagent can fire hundreds of read_file / bash child chips
        // — pushing those into the ring would evict the actually-
        // load-bearing checkpoint history before the next refresh
        // could replay it.  Keep child tool_start / tool_result
        // live-only; only the parent-level checkpoint stream is
        // persisted.
        let (bus, _rx, ring) = ringed_fixture();
        bus.tool_start("parent_42", "child_7", "bash");
        bus.tool_result(
            "parent_42",
            Some("child_7"),
            &ToolOutput::success("output"),
        );
        let snapshot = ring.lock().unwrap().since(0);
        assert!(
            snapshot.is_empty(),
            "child tool_start/tool_result must not enter the ring (would evict checkpoints): \
             ring had {} entries",
            snapshot.len(),
        );
    }

    #[test]
    fn checkpoint_without_replay_ring_still_broadcasts() {
        // The plain `new` constructor (used by tests and any caller
        // that doesn't want replay semantics) must still stream
        // checkpoints live.  Ring is opt-in via `with_replay_ring`.
        let (bus, mut rx) = fixture();
        bus.checkpoint("security_engineer: report");
        match rx.try_recv().expect("checkpoint must broadcast") {
            SseEvent::Checkpoint { text } => assert_eq!(text, "security_engineer: report"),
            other => panic!(
                "unexpected event: {}",
                serde_json::to_string(&other).unwrap()
            ),
        }
    }
}
