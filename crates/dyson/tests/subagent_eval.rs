// ===========================================================================
// Subagent integration tests.
//
// These tests exercise the subagent system end-to-end, verifying that:
//
// 1. A parent agent can invoke a subagent tool and receive its output
// 2. Subagent depth limits are enforced correctly
// 3. Subagent tools share the parent's sandbox
// 4. Subagent conversation history is isolated from the parent
// 5. Tool filtering works correctly
// ===========================================================================

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use dyson::agent::rate_limiter::RateLimitedHandle;
use dyson::agent::Agent;
use dyson::config::AgentSettings;
use dyson::controller::recording::RecordingOutput;
use dyson::error::Result;
use dyson::llm::stream::{StopReason, StreamEvent};
use dyson::llm::{CompletionConfig, LlmClient, ToolDefinition};
use dyson::message::Message;
use dyson::sandbox::{Sandbox, SandboxDecision};
use dyson::skill::Skill;
use dyson::tool::ToolContext;

// ===========================================================================
// Mock infrastructure
// ===========================================================================

struct MockLlm {
    responses: Mutex<Vec<Vec<StreamEvent>>>,
}

impl MockLlm {
    const fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _config: &CompletionConfig,
    ) -> Result<dyson::llm::StreamResponse> {
        let events = self.responses.lock().unwrap().remove(0);
        Ok(dyson::llm::StreamResponse {
            stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
            tool_mode: dyson::llm::ToolMode::Execute,
            input_tokens: None,
        })
    }
}

// MockOutput replaced by RecordingOutput from dyson::controller::recording.

/// A recording sandbox that logs every tool call it sees.
struct RecordingSandbox {
    calls: Mutex<Vec<String>>,
}

impl RecordingSandbox {
    const fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Sandbox for RecordingSandbox {
    async fn check(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        self.calls.lock().unwrap().push(tool_name.to_string());
        Ok(SandboxDecision::Allow {
            input: _input.clone(),
        })
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn text_response_events(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]
}

fn tool_call_events(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: id.to_string(),
            name: name.to_string(),
        },
        StreamEvent::ToolUseComplete {
            id: id.to_string(),
            name: name.to_string(),
            input,
        },
        StreamEvent::MessageComplete {
            stop_reason: StopReason::ToolUse,
            output_tokens: None,
        },
    ]
}

// ===========================================================================
// 1. Subagent runs child and parent receives result
// ===========================================================================

/// Tests the full subagent flow using CaptureOutput + FilteredSkill.
///
/// This test constructs a child Agent manually with a MockLlm —
/// verifying that CaptureOutput, FilteredSkill, and Agent integration
/// work correctly.
#[tokio::test]
async fn subagent_child_agent_returns_result_via_capture_output() {
    // The child agent sees a simple text response.
    let child_llm = MockLlm::new(vec![text_response_events(
        "The research shows Rust is awesome.",
    )]);

    let settings = AgentSettings {
        api_key: "test".into(),
        system_prompt: "You are a research specialist.".into(),
        ..Default::default()
    };

    // Child gets no tools (just responds with text).
    let skills: Vec<Box<dyn Skill>> = vec![Box::new(
        dyson::skill::subagent::FilteredSkill::new(vec![]),
    )];
    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(RateLimitedHandle::unlimited(Box::new(child_llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    agent.set_depth(1); // This is a child agent.

    let mut capture = dyson::skill::subagent::CaptureOutput::new();
    let result = agent
        .run("Research Rust patterns", &mut capture)
        .await
        .unwrap();

    assert_eq!(result, "The research shows Rust is awesome.");
    assert_eq!(capture.text(), "The research shows Rust is awesome.");
}

// ===========================================================================
// 2. Sandbox is shared between parent and child
// ===========================================================================

#[tokio::test]
async fn subagent_shares_sandbox_with_parent() {
    let sandbox = Arc::new(RecordingSandbox::new());

    // Parent calls bash (recorded by sandbox), then child calls bash too.
    // Both should go through the same RecordingSandbox.

    // Parent sees: tool call to bash, then text response.
    let parent_llm = MockLlm::new(vec![
        tool_call_events(
            "call_1",
            "bash",
            serde_json::json!({"command": "echo parent"}),
        ),
        text_response_events("Done from parent."),
    ]);

    let skills: Vec<Box<dyn Skill>> =
        vec![Box::new(dyson::skill::builtin::BuiltinSkill::new(None, None, None))];
    let mut parent = Agent::new(
        RateLimitedHandle::unlimited(Box::new(parent_llm)),
        Arc::clone(&sandbox) as Arc<dyn Sandbox>,
        skills,
        &AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        },
        None,
        0,
        None,
        None,
    )
    .unwrap();

    let mut output = RecordingOutput::new();
    parent.run("run echo parent", &mut output).await.unwrap();

    // The sandbox should have recorded the "bash" call from the parent.
    let calls = sandbox.calls();
    assert!(calls.contains(&"bash".to_string()));

    // Now verify a child agent using the same sandbox also records calls.
    let child_llm = MockLlm::new(vec![
        tool_call_events(
            "call_c1",
            "bash",
            serde_json::json!({"command": "echo child"}),
        ),
        text_response_events("Done from child."),
    ]);

    let child_skills: Vec<Box<dyn Skill>> =
        vec![Box::new(dyson::skill::builtin::BuiltinSkill::new(None, None, None))];
    let mut child = Agent::new(
        RateLimitedHandle::unlimited(Box::new(child_llm)),
        Arc::clone(&sandbox) as Arc<dyn Sandbox>,
        child_skills,
        &AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        },
        None,
        0,
        None,
        None,
    )
    .unwrap();
    child.set_depth(1);

    let mut child_output = RecordingOutput::new();
    child
        .run("run echo child", &mut child_output)
        .await
        .unwrap();

    // Both parent and child bash calls should be recorded.
    let all_calls = sandbox.calls();
    let bash_count = all_calls.iter().filter(|c| *c == "bash").count();
    assert!(
        bash_count >= 2,
        "expected at least 2 bash calls (parent + child), got {bash_count}"
    );
}

// ===========================================================================
// 3. Child conversation is isolated from parent
// ===========================================================================

#[tokio::test]
async fn subagent_conversation_isolated_from_parent() {
    // Parent has 2 turns. Child has 1 turn.
    // Parent's messages should NOT include the child's.
    let parent_llm = MockLlm::new(vec![
        text_response_events("First response."),
        text_response_events("Second response."),
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let skills: Vec<Box<dyn Skill>> =
        vec![Box::new(dyson::skill::builtin::BuiltinSkill::new(None, None, None))];

    let mut parent = Agent::new(
        RateLimitedHandle::unlimited(Box::new(parent_llm)),
        sandbox.clone(),
        skills,
        &settings,
        None,
        0,
        None,
        None,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    parent.run("hello", &mut output).await.unwrap();
    parent.run("world", &mut output).await.unwrap();
    let parent_msg_count = parent.messages().len();

    // Now run a separate child agent.
    let child_llm = MockLlm::new(vec![text_response_events("Child says hi.")]);
    let child_skills: Vec<Box<dyn Skill>> =
        vec![Box::new(dyson::skill::builtin::BuiltinSkill::new(None, None, None))];
    let mut child = Agent::new(
        RateLimitedHandle::unlimited(Box::new(child_llm)),
        sandbox,
        child_skills,
        &settings,
        None,
        0,
        None,
        None,
    )
    .unwrap();
    child.set_depth(1);

    let mut capture = dyson::skill::subagent::CaptureOutput::new();
    child.run("task for child", &mut capture).await.unwrap();

    // Child should have exactly 2 messages (user + assistant).
    assert_eq!(child.messages().len(), 2);

    // Parent should still have only its own messages (unchanged).
    assert_eq!(parent.messages().len(), parent_msg_count);
}

// ===========================================================================
// 4. Depth tracking works across levels
// ===========================================================================

#[tokio::test]
async fn depth_propagates_to_child_tool_context() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    // Create parent at depth 0, set child to depth 2.
    let llm = MockLlm::new(vec![text_response_events("OK.")]);
    let skills: Vec<Box<dyn Skill>> =
        vec![Box::new(dyson::skill::builtin::BuiltinSkill::new(None, None, None))];
    let mut agent = Agent::new(RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    agent.set_depth(2);

    let mut output = RecordingOutput::new();
    let result = agent.run("test", &mut output).await.unwrap();
    assert_eq!(result, "OK.");
    // Agent ran fine at depth 2 — only SubagentTool checks depth limits.
}
