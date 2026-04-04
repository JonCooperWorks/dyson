// ===========================================================================
// Agent evaluation tests.
//
// These tests exercise the agent loop end-to-end using mock LLM clients and
// sandboxes.  They verify correctness of:
//
// 1. Sandbox deny flow — tool calls blocked by sandbox return errors to LLM
// 2. Sandbox redirect flow — tool calls transparently rerouted to another tool
// 3. Max iterations — agent stops after hitting the iteration limit
// 4. Conversation persistence — messages accumulate across run() calls
// 5. Multiple tool calls per turn — agent handles parallel tool invocations
// 6. Unknown tool handling — graceful error when LLM requests nonexistent tool
// 7. Multi-turn context — conversation history flows correctly
// 8. Agent clear — conversation reset works
// 9. Tool execution errors — tool failures are reported back to the LLM
// ===========================================================================

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use dyson::agent::Agent;
use dyson::config::AgentSettings;
use dyson::controller::recording::RecordingOutput;
use dyson::error::Result;
use dyson::llm::stream::{StopReason, StreamEvent};
use dyson::llm::{CompletionConfig, LlmClient, ToolDefinition};
use dyson::message::Message;
use dyson::sandbox::{Sandbox, SandboxDecision};
use dyson::skill::Skill;
use dyson::skill::builtin::BuiltinSkill;
use dyson::tool::ToolContext;

// ===========================================================================
// Mock infrastructure
// ===========================================================================

// ---------------------------------------------------------------------------
// MockLlm — returns pre-programmed responses.
// ---------------------------------------------------------------------------

struct MockLlm {
    responses: Mutex<Vec<Vec<StreamEvent>>>,
}

impl MockLlm {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
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

// ---------------------------------------------------------------------------
// DenySandbox — denies all tool calls.
// ---------------------------------------------------------------------------

struct DenySandbox {
    reason: String,
}

impl DenySandbox {
    fn new(reason: &str) -> Self {
        Self {
            reason: reason.to_string(),
        }
    }
}

#[async_trait]
impl Sandbox for DenySandbox {
    async fn check(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        Ok(SandboxDecision::Deny {
            reason: self.reason.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// RedirectSandbox — redirects tool calls to a different tool.
// ---------------------------------------------------------------------------

struct RedirectSandbox {
    target_tool: String,
}

impl RedirectSandbox {
    fn new(target: &str) -> Self {
        Self {
            target_tool: target.to_string(),
        }
    }
}

#[async_trait]
impl Sandbox for RedirectSandbox {
    async fn check(
        &self,
        _tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        Ok(SandboxDecision::Redirect {
            tool_name: self.target_tool.clone(),
            input: input.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// SelectiveDenySandbox — denies specific tools, allows others.
// ---------------------------------------------------------------------------

struct SelectiveDenySandbox {
    denied_tools: Vec<String>,
}

impl SelectiveDenySandbox {
    fn new(denied: &[&str]) -> Self {
        Self {
            denied_tools: denied.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[async_trait]
impl Sandbox for SelectiveDenySandbox {
    async fn check(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<SandboxDecision> {
        if self.denied_tools.contains(&tool_name.to_string()) {
            Ok(SandboxDecision::Deny {
                reason: format!("{tool_name} is not allowed"),
            })
        } else {
            Ok(SandboxDecision::Allow {
                input: input.clone(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_settings() -> AgentSettings {
    AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    }
}

fn builtin_skills() -> Vec<Box<dyn Skill>> {
    vec![Box::new(BuiltinSkill::new(None))]
}

fn tool_call_events(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: id.into(),
            name: name.into(),
        },
        StreamEvent::ToolUseComplete {
            id: id.into(),
            name: name.into(),
            input,
        },
        StreamEvent::MessageComplete {
            stop_reason: StopReason::ToolUse,
            output_tokens: None,
        },
    ]
}

fn text_response_events(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]
}

// ===========================================================================
// 1. Sandbox deny flow
// ===========================================================================

#[tokio::test]
async fn sandbox_deny_returns_error_to_llm() {
    // The LLM calls bash, but the sandbox denies it.
    // The agent should send the denial reason back as a tool_result error,
    // then the LLM responds with text on the next iteration.
    let llm = MockLlm::new(vec![
        tool_call_events("call_1", "bash", serde_json::json!({"command": "rm -rf /"})),
        text_response_events("I can't do that."),
    ]);

    let sandbox = DenySandbox::new("dangerous command blocked");
    let mut agent = Agent::new(
        Box::new(llm),
        Arc::new(sandbox),
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("delete everything", &mut output).await.unwrap();
    assert_eq!(result, "I can't do that.");

    // The denied tool result should have been reported to the output.
    assert!(
        output.tool_results().iter().any(|(content, is_error)| {
            *is_error && content.contains("dangerous command blocked")
        })
    );
}

// ===========================================================================
// 2. Sandbox redirect flow
// ===========================================================================

#[tokio::test]
async fn sandbox_redirect_routes_to_different_tool() {
    // The LLM calls "bash", but the sandbox redirects to "workspace_view"
    // (which is a valid tool in BuiltinSkill).
    let llm = MockLlm::new(vec![
        tool_call_events(
            "call_1",
            "bash",
            serde_json::json!({"command": "cat SOUL.md"}),
        ),
        text_response_events("Redirected successfully."),
    ]);

    let sandbox = RedirectSandbox::new("workspace_view");
    let mut agent = Agent::new(
        Box::new(llm),
        Arc::new(sandbox),
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("read soul file", &mut output).await.unwrap();
    assert_eq!(result, "Redirected successfully.");

    // The tool result should exist (the redirected tool was called).
    assert!(!output.tool_results().is_empty());
}

// ===========================================================================
// 3. Max iterations limit
// ===========================================================================

#[tokio::test]
async fn agent_stops_at_max_iterations() {
    // Create an LLM that always calls tools (never stops on its own).
    // With max_iterations=3, the agent should make exactly 3 LLM calls,
    // then a 4th summary call (no tools) to wrap up gracefully.
    let mut responses = Vec::new();
    for i in 0..3 {
        responses.push(tool_call_events(
            &format!("call_{i}"),
            "bash",
            serde_json::json!({"command": "echo loop"}),
        ));
    }
    // The summary call that fires after max iterations is reached.
    responses.push(text_response_events("Here is a summary of progress."));

    let llm = MockLlm::new(responses);
    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut settings = default_settings();
    settings.max_iterations = 3;

    let mut agent =
        Agent::new(Box::new(llm), sandbox, builtin_skills(), &settings, None, 0).unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("loop forever", &mut output).await.unwrap();

    // The summary text should be returned as the final result.
    assert!(
        result.contains("summary of progress"),
        "agent should return a summary after hitting max iterations, got: {result}"
    );
}

// ===========================================================================
// 4. Conversation persistence across run() calls
// ===========================================================================

#[tokio::test]
async fn conversation_persists_across_runs() {
    // Run the agent twice.  After the second run, the message history
    // should contain messages from both turns.
    let llm = MockLlm::new(vec![
        text_response_events("First response."),
        text_response_events("Second response."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();

    let mut output1 = RecordingOutput::new();
    agent.run("first question", &mut output1).await.unwrap();

    let mut output2 = RecordingOutput::new();
    agent.run("second question", &mut output2).await.unwrap();

    // Should have 4 messages: user1, assistant1, user2, assistant2.
    let messages = agent.messages();
    assert_eq!(messages.len(), 4, "should have 4 messages after 2 turns");
}

// ===========================================================================
// 5. Multiple tool calls in a single turn
// ===========================================================================

#[tokio::test]
async fn multiple_tool_calls_in_one_turn() {
    // The LLM emits two tool calls in the same turn.
    let llm = MockLlm::new(vec![
        // Turn 1: two tool calls.
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hello"}),
            },
            StreamEvent::ToolUseStart {
                id: "call_2".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_2".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo world"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        // Turn 2: text response.
        text_response_events("Both commands ran."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("run both", &mut output).await.unwrap();
    assert_eq!(result, "Both commands ran.");

    // Both tool calls should have produced results.
    assert_eq!(output.tool_results().len(), 2, "should have 2 tool results");

    // Conversation: user, assistant(2 tools), tool_result_1, tool_result_2, assistant(text).
    assert_eq!(agent.messages().len(), 5);
}

// ===========================================================================
// 6. Unknown tool handling
// ===========================================================================

#[tokio::test]
async fn unknown_tool_returns_error() {
    // The LLM requests a tool that doesn't exist ("nonexistent_tool").
    // The agent catches the error and sends it back as a tool_result message.
    let llm = MockLlm::new(vec![
        tool_call_events(
            "call_1",
            "nonexistent_tool",
            serde_json::json!({"input": "test"}),
        ),
        text_response_events("Tool not found, sorry."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent
        .run("use nonexistent tool", &mut output)
        .await
        .unwrap();
    assert_eq!(result, "Tool not found, sorry.");

    // The error goes into the conversation as a tool_result message (not
    // through output.tool_result, since execute_tool_call returns Err which
    // the run loop catches).  Verify the error is in the conversation history.
    let messages = agent.messages();
    let has_error_result = messages.iter().any(|msg| {
        msg.content.iter().any(|block| {
            if let dyson::message::ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            {
                *is_error && content.to_lowercase().contains("unknown")
            } else {
                false
            }
        })
    });
    assert!(
        has_error_result,
        "conversation should contain an error tool_result for unknown tool"
    );
}

// ===========================================================================
// 7. Multi-turn context
// ===========================================================================

#[tokio::test]
async fn multi_turn_builds_correct_history() {
    // Turn 1: text only.  Turn 2: tool call then text.
    let llm = MockLlm::new(vec![
        // Turn 1 response.
        text_response_events("I understand."),
        // Turn 2 response: tool call.
        tool_call_events("call_1", "bash", serde_json::json!({"command": "ls"})),
        // Turn 2 response: final text.
        text_response_events("Here are the files."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();

    let mut out1 = RecordingOutput::new();
    agent.run("hello", &mut out1).await.unwrap();

    let mut out2 = RecordingOutput::new();
    let result = agent.run("list files", &mut out2).await.unwrap();
    assert_eq!(result, "Here are the files.");

    // After turn 1: user, assistant (2 messages).
    // After turn 2: user, assistant(tool), tool_result, assistant(text) (+4 = 6 total).
    assert_eq!(agent.messages().len(), 6);
}

// ===========================================================================
// 8. Agent clear resets conversation
// ===========================================================================

#[tokio::test]
async fn clear_resets_conversation_history() {
    let llm = MockLlm::new(vec![
        text_response_events("First."),
        text_response_events("After clear."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();

    let mut out1 = RecordingOutput::new();
    agent.run("hello", &mut out1).await.unwrap();
    assert_eq!(agent.messages().len(), 2);

    agent.clear();
    assert_eq!(agent.messages().len(), 0, "clear should empty all messages");

    let mut out2 = RecordingOutput::new();
    agent.run("fresh start", &mut out2).await.unwrap();
    assert_eq!(
        agent.messages().len(),
        2,
        "after clear + one turn should have 2 messages"
    );
}

// ===========================================================================
// 9. Tool execution errors are reported to LLM
// ===========================================================================

#[tokio::test]
async fn tool_error_is_reported_back() {
    // Use bash with an invalid command to trigger an error output.
    let llm = MockLlm::new(vec![
        tool_call_events(
            "call_1",
            "bash",
            serde_json::json!({"command": "nonexistent_command_that_does_not_exist_xyz"}),
        ),
        text_response_events("Command failed."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("run bad command", &mut output).await.unwrap();
    assert_eq!(result, "Command failed.");

    // The tool result should be an error.
    assert!(
        output.tool_results().iter().any(|(_, is_error)| *is_error),
        "failed command should produce an error tool result"
    );
}

// ===========================================================================
// 10. Selective sandbox: some tools denied, others allowed
// ===========================================================================

#[tokio::test]
async fn selective_sandbox_denies_specific_tools() {
    // The LLM first calls "bash" (denied), then on the next turn calls
    // "bash" again (also denied), then responds with text.
    // This tests that the sandbox selectively blocks tools while the agent
    // loop continues functioning.
    let llm = MockLlm::new(vec![
        tool_call_events("call_1", "bash", serde_json::json!({"command": "ls"})),
        text_response_events("Bash was denied, I'll answer directly."),
    ]);

    let sandbox = SelectiveDenySandbox::new(&["bash"]);
    let mut agent = Agent::new(
        Box::new(llm),
        Arc::new(sandbox),
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("list things", &mut output).await.unwrap();
    assert_eq!(result, "Bash was denied, I'll answer directly.");

    // The tool result should be an error (bash denied).
    let results = output.tool_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "bash should be denied");
    assert!(
        results[0].0.contains("not allowed"),
        "denial message should explain why: {}",
        results[0].0
    );
}

// ===========================================================================
// 11. Text-only response with no tools
// ===========================================================================

#[tokio::test]
async fn simple_text_response_no_tools() {
    let llm = MockLlm::new(vec![text_response_events("Hello, world!")]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("hi", &mut output).await.unwrap();
    assert_eq!(result, "Hello, world!");
    assert_eq!(output.text(), "Hello, world!");
    assert!(output.tool_calls().is_empty());
    assert!(output.tool_results().is_empty());
    assert_eq!(agent.messages().len(), 2);
}

// ===========================================================================
// 12. Set/restore messages preserves conversation
// ===========================================================================

#[tokio::test]
async fn set_messages_restores_conversation() {
    let llm = MockLlm::new(vec![text_response_events("Continuing.")]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();

    // Inject a pre-existing conversation.
    let history = vec![
        Message::user("previous question"),
        Message::assistant(vec![dyson::message::ContentBlock::Text {
            text: "previous answer".into(),
        }]),
    ];
    agent.set_messages(history);
    assert_eq!(agent.messages().len(), 2);

    let mut output = RecordingOutput::new();
    agent.run("follow up", &mut output).await.unwrap();

    // Should have: prev_user, prev_assistant, new_user, new_assistant = 4.
    assert_eq!(agent.messages().len(), 4);
}

// ===========================================================================
// 13. Redirect to unknown tool produces error
// ===========================================================================

#[tokio::test]
async fn redirect_to_unknown_tool_is_handled_gracefully() {
    let llm = MockLlm::new(vec![
        tool_call_events("call_1", "bash", serde_json::json!({"command": "ls"})),
        text_response_events("Redirect failed."),
    ]);

    // Redirect all calls to a tool that doesn't exist.
    let sandbox = RedirectSandbox::new("tool_that_does_not_exist");
    let mut agent = Agent::new(
        Box::new(llm),
        Arc::new(sandbox),
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    // The agent catches the unknown-tool error and sends it back to the LLM
    // as a tool_result error.  The run() itself succeeds.
    let result = agent.run("try redirect", &mut output).await.unwrap();
    assert_eq!(result, "Redirect failed.");

    // The error should be in the conversation history as an error tool_result.
    let has_redirect_error = agent.messages().iter().any(|msg| {
        msg.content.iter().any(|block| {
            if let dyson::message::ContentBlock::ToolResult {
                content, is_error, ..
            } = block
            {
                *is_error && content.contains("unknown tool")
            } else {
                false
            }
        })
    });
    assert!(
        has_redirect_error,
        "conversation should contain error about unknown redirected tool"
    );
}

// ===========================================================================
// 14. Streaming text accumulation with multiple deltas
// ===========================================================================

#[tokio::test]
async fn streaming_text_accumulates_correctly() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Hello".into()),
        StreamEvent::TextDelta(", ".into()),
        StreamEvent::TextDelta("world".into()),
        StreamEvent::TextDelta("!".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("say hello world", &mut output).await.unwrap();
    assert_eq!(result, "Hello, world!");
    assert_eq!(output.text(), "Hello, world!");
}

// ===========================================================================
// 15. Concurrent tool execution
// ===========================================================================

#[tokio::test]
async fn concurrent_tool_calls_produce_all_results() {
    // The LLM emits 3 tool calls. All should execute and produce results,
    // verifying that concurrent dispatch works correctly.
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_a".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_a".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo alpha"}),
            },
            StreamEvent::ToolUseStart {
                id: "call_b".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_b".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo bravo"}),
            },
            StreamEvent::ToolUseStart {
                id: "call_c".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_c".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo charlie"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        text_response_events("All three done."),
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("run three", &mut output).await.unwrap();
    assert_eq!(result, "All three done.");

    // All 3 tool calls should have produced results.
    assert_eq!(
        output.tool_results().len(),
        3,
        "all 3 concurrent tools should produce results"
    );

    // Conversation: user, assistant(3 tools), tool_result x3, assistant(text) = 6 messages.
    assert_eq!(agent.messages().len(), 6);
}

// ===========================================================================
// 16. Token budget enforcement via integration test
// ===========================================================================

#[tokio::test]
async fn token_budget_halts_agent_after_limit() {
    // Budget: 50 tokens. LLM reports 40 per turn.
    // Turn 1: tool call (40 tokens, total 40, under budget → continue).
    // Turn 2: response (40 tokens, total 80, over budget → stop).
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: Some(40),
            },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: Some(40),
            },
        ],
    ]);

    let sandbox: Arc<dyn Sandbox> = Arc::new(dyson::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent = Agent::new(
        Box::new(llm),
        sandbox,
        builtin_skills(),
        &default_settings(),
        None,
        0,
    )
    .unwrap();
    agent.token_budget.max_output_tokens = Some(50);
    let mut output = RecordingOutput::new();

    // Should complete (budget exceeded mid-loop triggers break, not hard error).
    let _result = agent.run("test budget", &mut output).await.unwrap();

    // Token budget should reflect usage.
    assert_eq!(agent.token_budget.output_tokens_used, 80);
    assert_eq!(agent.token_budget.llm_calls, 2);
    assert!(!agent.token_budget.has_budget());

    // An error about exceeding budget should have been surfaced.
    assert!(
        output.errors().iter().any(|e| e.contains("token budget")),
        "should surface token budget error, got: {:?}",
        output.errors()
    );
}
