// ===========================================================================
// Tool Hooks — pre/post tool execution lifecycle hooks.
//
// Provides a hook system that runs before and after each tool execution,
// allowing hooks to block, modify, or observe tool calls.
// ===========================================================================

use std::time::Duration;

use crate::agent::stream_handler::ToolCall;
use crate::error::DysonError;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// ToolHookEvent
// ---------------------------------------------------------------------------

/// Events dispatched to hooks during tool execution.
pub enum ToolHookEvent<'a> {
    /// Fired before a tool is executed. Hooks can block or modify the call.
    PreToolUse { call: &'a ToolCall },

    /// Fired after a tool completes successfully.
    PostToolUse {
        output: &'a ToolOutput,
        duration: Duration,
    },

    /// Fired after a tool execution fails with an error.
    PostToolUseFailure { error: &'a DysonError },
}

// ---------------------------------------------------------------------------
// HookDecision
// ---------------------------------------------------------------------------

/// The decision returned by a hook in response to an event.
#[derive(Debug)]
pub enum HookDecision {
    /// Allow the tool call to proceed unchanged.
    Allow,

    /// Block the tool call with a reason.
    Block { reason: String },

    /// Allow the tool call but with modified input.
    Modify { input: serde_json::Value },
}

// ---------------------------------------------------------------------------
// ToolHook trait
// ---------------------------------------------------------------------------

/// A lifecycle hook that can observe, block, or modify tool executions.
pub trait ToolHook: Send + Sync {
    /// Handle a tool hook event and return a decision.
    ///
    /// For `PostToolUse` and `PostToolUseFailure` events, the decision
    /// is ignored (they're observational).  Only `PreToolUse` decisions
    /// affect control flow.
    fn on_event(&self, event: &ToolHookEvent) -> HookDecision;
}

// ---------------------------------------------------------------------------
// dispatch_hooks
// ---------------------------------------------------------------------------

/// Dispatch an event to a list of hooks, returning the aggregate decision.
///
/// For `PreToolUse` events, the first `Block` or `Modify` wins.
/// If all hooks return `Allow`, the result is `Allow`.
///
/// For `PostToolUse` and `PostToolUseFailure`, all hooks are called
/// (for observation) and `Allow` is always returned.
pub fn dispatch_hooks(hooks: &[Box<dyn ToolHook>], event: &ToolHookEvent) -> HookDecision {
    let is_pre = matches!(event, ToolHookEvent::PreToolUse { .. });

    for hook in hooks {
        let decision = hook.on_event(event);
        if is_pre {
            match decision {
                HookDecision::Allow => continue,
                HookDecision::Block { .. } | HookDecision::Modify { .. } => return decision,
            }
        }
    }

    HookDecision::Allow
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod test_tool_hooks {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- Test hook implementations --

    struct AllowAllHook;
    impl ToolHook for AllowAllHook {
        fn on_event(&self, _event: &ToolHookEvent) -> HookDecision {
            HookDecision::Allow
        }
    }

    struct BlockDangerousHook;
    impl ToolHook for BlockDangerousHook {
        fn on_event(&self, event: &ToolHookEvent) -> HookDecision {
            if let ToolHookEvent::PreToolUse { call } = event {
                if let Some(cmd) = call.input.get("command").and_then(|v| v.as_str()) {
                    if cmd.contains("rm -rf") {
                        return HookDecision::Block {
                            reason: "dangerous command blocked".to_string(),
                        };
                    }
                }
            }
            HookDecision::Allow
        }
    }

    struct AddTimeoutHook;
    impl ToolHook for AddTimeoutHook {
        fn on_event(&self, event: &ToolHookEvent) -> HookDecision {
            if let ToolHookEvent::PreToolUse { call } = event {
                if call.name == "bash" {
                    let mut input = call.input.clone();
                    input["timeout"] = json!(30);
                    return HookDecision::Modify { input };
                }
            }
            HookDecision::Allow
        }
    }

    struct RecordingHook {
        count: AtomicUsize,
    }

    impl RecordingHook {
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.count.load(Ordering::Relaxed)
        }
    }

    impl ToolHook for RecordingHook {
        fn on_event(&self, _event: &ToolHookEvent) -> HookDecision {
            self.count.fetch_add(1, Ordering::Relaxed);
            HookDecision::Allow
        }
    }

    impl ToolHook for Arc<RecordingHook> {
        fn on_event(&self, event: &ToolHookEvent) -> HookDecision {
            (**self).on_event(event)
        }
    }

    // -- Tests --

    #[test]
    fn allows_when_no_hooks_block() {
        let hooks: Vec<Box<dyn ToolHook>> = vec![Box::new(AllowAllHook)];
        let call = ToolCall::new("bash", json!({"command": "echo hi"}));
        assert!(matches!(
            dispatch_hooks(&hooks, &ToolHookEvent::PreToolUse { call: &call }),
            HookDecision::Allow
        ));
    }

    #[test]
    fn blocks_dangerous_commands() {
        let hooks: Vec<Box<dyn ToolHook>> = vec![Box::new(BlockDangerousHook)];
        let call = ToolCall::new("bash", json!({"command": "rm -rf /tmp"}));
        assert!(matches!(
            dispatch_hooks(&hooks, &ToolHookEvent::PreToolUse { call: &call }),
            HookDecision::Block { .. }
        ));
    }

    #[test]
    fn modifies_tool_input() {
        let hooks: Vec<Box<dyn ToolHook>> = vec![Box::new(AddTimeoutHook)];
        let call = ToolCall::new("bash", json!({"command": "sleep 100"}));
        let r = dispatch_hooks(&hooks, &ToolHookEvent::PreToolUse { call: &call });
        if let HookDecision::Modify { input } = r {
            assert_eq!(input["timeout"], 30);
        } else {
            panic!("Expected Modify");
        }
    }

    #[test]
    fn fires_post_tool_use_hooks() {
        let hook = Arc::new(RecordingHook::new());
        let hooks: Vec<Box<dyn ToolHook>> = vec![Box::new(Arc::clone(&hook))];
        dispatch_hooks(
            &hooks,
            &ToolHookEvent::PostToolUse {
                output: &ToolOutput::success("ok"),
                duration: Duration::from_millis(50),
            },
        );
        assert_eq!(hook.call_count(), 1);
    }

    #[test]
    fn fires_post_failure_hooks() {
        let hook = Arc::new(RecordingHook::new());
        let hooks: Vec<Box<dyn ToolHook>> = vec![Box::new(Arc::clone(&hook))];
        dispatch_hooks(
            &hooks,
            &ToolHookEvent::PostToolUseFailure {
                error: &DysonError::Tool {
                    tool: "bash".into(),
                    message: "timeout".into(),
                },
            },
        );
        assert_eq!(hook.call_count(), 1);
    }
}
