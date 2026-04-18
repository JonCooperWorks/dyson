use super::*;
use crate::llm::stream::{StopReason, StreamEvent};
use crate::message::{ContentBlock, Role};
use crate::sandbox::no_sandbox::DangerousNoSandbox;
use crate::skill::builtin::BuiltinSkill;
use crate::tool::ToolOutput;

// -----------------------------------------------------------------------
// Mock LLM client that returns a fixed response.
// -----------------------------------------------------------------------

struct MockLlm {
    /// Responses to return, in order.  Each call to `stream()` pops
    /// the first entry.
    responses: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
    /// Simulate a provider that handles tools internally (like Claude Code).
    tool_mode: crate::llm::ToolMode,
}

impl MockLlm {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            tool_mode: crate::llm::ToolMode::Execute,
        }
    }

    fn with_internal_tools(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            tool_mode: crate::llm::ToolMode::Observe,
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for MockLlm {
    async fn stream(
        &self,
        _messages: &[Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[ToolDefinition],
        _config: &CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        let events = self.responses.lock().unwrap().remove(0);
        Ok(crate::llm::StreamResponse {
            stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
            tool_mode: self.tool_mode,
            input_tokens: None,
        })
    }
}

use crate::controller::recording::RecordingOutput;

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn simple_text_response() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Hello!".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("hi", &mut output).await.unwrap();
    assert_eq!(result, "Hello!");
    assert_eq!(output.text(), "Hello!");
}

#[tokio::test]
async fn tool_call_loop() {
    // First LLM call: request a bash command.
    // Second LLM call: respond with the result.
    let llm = MockLlm::new(vec![
        // Turn 1: LLM calls bash.
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo test_output"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        // Turn 2: LLM responds with text.
        vec![
            StreamEvent::TextDelta("Done.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    let result = agent
        .run("run echo test_output", &mut output)
        .await
        .unwrap();
    assert_eq!(result, "Done.");

    // Conversation should have: user, assistant (tool_use), tool_result, assistant (text)
    assert_eq!(agent.conversation.messages.len(), 4);
}

#[tokio::test]
async fn internal_tools_provider_skips_tool_execution() {
    // Simulate a provider like Claude Code that handles tools internally.
    // The stream includes tool events, but the agent loop should NOT try
    // to execute them — it should break after one iteration.
    let llm = MockLlm::with_internal_tools(vec![vec![
        StreamEvent::TextDelta("I'll check. ".into()),
        StreamEvent::ToolUseStart {
            id: "cc_1".into(),
            name: "bash".into(),
        },
        StreamEvent::ToolUseComplete {
            id: "cc_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        StreamEvent::TextDelta("Here are the files.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("list files", &mut output).await.unwrap();

    // Should get the final text, NOT an error from trying to execute "bash".
    assert_eq!(result, "Here are the files.");
    // Only 2 messages: user + assistant (no tool_result messages).
    assert_eq!(agent.conversation.messages.len(), 2);
}

#[tokio::test]
async fn memory_system_prompt_contains_usage_stats_and_curation_rules() {
    let ws = crate::workspace::InMemoryWorkspace::new()
        .with_overflow_factor(1.35)
        .with_file("MEMORY.md", "some memories here")
        .with_limit("MEMORY.md", 2500)
        .with_file("USER.md", "user info")
        .with_limit("USER.md", 1375);

    let workspace: Box<dyn crate::workspace::Workspace> = Box::new(ws);
    let ctx = crate::tool::ToolContext {
        working_dir: std::env::temp_dir(),
        env: HashMap::new(),
        cancellation: CancellationToken::new(),
        workspace: Some(std::sync::Arc::new(tokio::sync::RwLock::new(workspace))),
        depth: 0,
        dangerous_no_sandbox: false,
        taint_indexes: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    };

    let prompt = reflection::build_memory_system_prompt(&ctx).await;
    // Soft target + hard ceiling both reported.
    assert!(prompt.contains("MEMORY.md"));
    assert!(prompt.contains("soft target 2500"));
    // 2500 * 1.35 = 3375.
    assert!(prompt.contains("hard ceiling 3375"));
    assert!(prompt.contains("USER.md"));
    assert!(prompt.contains("soft target 1375"));
    assert!(prompt.contains("memory_search"));
    assert!(prompt.contains("workspace"));
    // Curation rules must be embedded.
    assert!(prompt.contains("KEEP"));
    assert!(prompt.contains("DISCARD"));
    assert!(
        prompt.contains("anti-timestamp rule"),
        "night work must be protected"
    );
}

#[tokio::test]
async fn reflection_system_prompt_lists_tools() {
    let ctx = crate::tool::ToolContext {
        working_dir: std::env::temp_dir(),
        env: HashMap::new(),
        cancellation: CancellationToken::new(),
        workspace: None,
        depth: 0,
        dangerous_no_sandbox: false,
        taint_indexes: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    };
    let prompt = reflection::build_reflection_system_prompt(&ctx).await;
    assert!(prompt.contains("skill_create"));
    assert!(!prompt.contains("export_conversation"));
    assert!(prompt.contains("When to create a skill"));
    assert!(prompt.contains("When to do nothing"));
    assert!(prompt.contains("Rating-informed decisions"));
}

#[test]
fn summarize_for_reflection_captures_tool_stats() {
    let messages = vec![
        Message::user("Deploy my app"),
        Message::assistant(vec![
            crate::message::ContentBlock::Text {
                text: "I'll deploy it.".into(),
            },
            crate::message::ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "deploy.sh"}),
            },
        ]),
        Message::tool_result("c1", "Deployed successfully", false),
        Message::assistant(vec![crate::message::ContentBlock::Text {
            text: "Done!".into(),
        }]),
    ];

    let summary = reflection::summarize_for_reflection(&messages);
    assert!(summary.contains("Deploy my app"));
    assert!(summary.contains("[Tool call: bash]"));
    assert!(summary.contains("Deployed successfully"));
    assert!(summary.contains("1 tool calls (0 errors)"));
    assert!(summary.contains("bash"));
    assert!(summary.contains("4 messages total"));
}

#[test]
fn summarize_for_reflection_handles_multibyte_utf8() {
    // Regression: slicing at byte 200 or 500 can land inside a
    // multi-byte character (e.g. smart quotes '\u{2019}' is 3 bytes).
    // Build a string where byte 200 is mid-character.
    let mut text = "a".repeat(199);
    text.push('\u{2019}'); // RIGHT SINGLE QUOTATION MARK — 3 bytes (bytes 199..202)
    text.push_str("end");
    assert_eq!(text.len(), 205); // 199 + 3 + 3

    let messages = vec![Message::user(&text)];

    // Should not panic.
    let summary = reflection::summarize_for_reflection(&messages);
    assert!(summary.contains(&"a".repeat(199)));

    // Same for tool error content (truncated at 200 bytes).
    let error_content = text.clone();
    let messages_with_error = vec![
        Message::assistant(vec![crate::message::ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]),
        Message::tool_result("t1", &error_content, true),
    ];

    let summary = reflection::summarize_for_reflection(&messages_with_error);
    assert!(summary.contains("[Tool error:"));

    // And for non-error tool results (also truncated at 200).
    let messages_with_result = vec![
        Message::assistant(vec![crate::message::ContentBlock::ToolUse {
            id: "t2".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]),
        Message::tool_result("t2", &error_content, false),
    ];

    let summary = reflection::summarize_for_reflection(&messages_with_result);
    assert!(summary.contains("[Tool result:"));
}

// -----------------------------------------------------------------------
// Feedback summary formatting tests
// -----------------------------------------------------------------------

fn fb(turn_index: usize, rating: crate::feedback::FeedbackRating) -> crate::feedback::FeedbackEntry {
    crate::feedback::FeedbackEntry {
        turn_index,
        rating,
        score: rating.score(),
        timestamp: 0,
    }
}

#[test]
fn format_feedback_summary_empty_returns_empty() {
    assert!(reflection::format_feedback_summary(&[], 10).is_empty());
}

#[test]
fn format_feedback_summary_mixed_ratings() {
    use crate::feedback::FeedbackRating::*;

    let entries = vec![fb(3, Excellent), fb(5, Bad), fb(7, VeryGood), fb(9, Good)];
    let summary = reflection::format_feedback_summary(&entries, 20);
    assert!(summary.contains("4 rated turns out of 20 messages"));
    assert!(summary.contains("Highly rated (score >= +2): turns 3, 7"));
    assert!(summary.contains("Poorly rated (score <= -1): turns 5"));
    assert!(summary.contains("Score distribution:"));
}

#[test]
fn format_feedback_summary_all_positive() {
    use crate::feedback::FeedbackRating::*;

    let entries = vec![fb(1, Good), fb(3, Good)];
    let summary = reflection::format_feedback_summary(&entries, 8);
    assert!(summary.contains("+1.0"));
    assert!(!summary.contains("Poorly rated"));
}

// -----------------------------------------------------------------------
// Background learning synthesis tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn synthesize_to_workspace_updates_memory() {
    let ws = crate::workspace::InMemoryWorkspace::new()
        .with_file("MEMORY.md", "Old memory content.");

    let workspace: crate::workspace::WorkspaceHandle =
        Arc::new(tokio::sync::RwLock::new(Box::new(ws)));

    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Updated memory with new learnings.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let config = CompletionConfig {
        model: "test".to_string(),
        max_tokens: 1024,
        temperature: None,
        api_tool_injections: vec![],
    };

    let summary = "User asked about Rust lifetimes and learned about borrowing.";

    let result = reflection::synthesize_to_workspace(&llm, &config, summary, &workspace).await;

    assert!(result.is_ok(), "synthesis should succeed");

    let ws = workspace.read().await;
    let memory = ws.get("MEMORY.md").unwrap();
    assert_eq!(memory, "Updated memory with new learnings.");
}

// -----------------------------------------------------------------------
// TokenBudget tests
// -----------------------------------------------------------------------

#[test]
fn token_budget_unlimited_by_default() {
    let budget = TokenBudget::default();
    assert!(budget.has_budget());
    assert_eq!(budget.output_tokens_used, 0);
    assert_eq!(budget.llm_calls, 0);
}

#[test]
fn token_budget_records_and_enforces() {
    let mut budget = TokenBudget {
        max_output_tokens: Some(100),
        ..TokenBudget::default()
    };

    // Under budget — should succeed.
    assert!(budget.record(50).is_ok());
    assert_eq!(budget.output_tokens_used, 50);
    assert_eq!(budget.llm_calls, 1);
    assert!(budget.has_budget());

    // Still under — should succeed.
    assert!(budget.record(49).is_ok());
    assert_eq!(budget.output_tokens_used, 99);
    assert!(budget.has_budget());

    // Over budget — should fail.
    assert!(budget.record(10).is_err());
    assert_eq!(budget.output_tokens_used, 109);
    assert!(!budget.has_budget());
}

#[test]
fn token_budget_reset() {
    let mut budget = TokenBudget {
        max_output_tokens: Some(100),
        ..TokenBudget::default()
    };
    budget.record(80).unwrap();
    assert_eq!(budget.llm_calls, 1);

    budget.reset();
    assert_eq!(budget.output_tokens_used, 0);
    assert_eq!(budget.llm_calls, 0);
    assert!(budget.has_budget());
}

#[test]
fn token_budget_unlimited_never_fails() {
    let mut budget = TokenBudget::default();
    // No max set — should always succeed.
    for _ in 0..100 {
        assert!(budget.record(1_000_000).is_ok());
    }
    assert!(budget.has_budget());
}

// -----------------------------------------------------------------------
// Retry logic tests
// -----------------------------------------------------------------------

#[test]
fn retryable_error_detection() {
    // Structured retryable variants.
    assert!(Agent::is_retryable(&DysonError::LlmRateLimit("rate limited".into())));
    assert!(Agent::is_retryable(&DysonError::LlmOverloaded("overloaded".into())));

    // Generic LLM errors are NOT retryable — retryable errors should use
    // the structured variants.
    assert!(!Agent::is_retryable(&DysonError::Llm(
        "authentication failed".into()
    )));
    assert!(!Agent::is_retryable(&DysonError::Config(
        "bad config".into()
    )));
    assert!(!Agent::is_retryable(&DysonError::Cancelled));
}

#[tokio::test]
async fn token_budget_stops_agent_loop() {
    // LLM reports 100 tokens per turn. Budget is 150, so it should
    // stop after the second turn (100 + 100 = 200 > 150).
    let llm = MockLlm::new(vec![
        // Turn 1: tool call (100 tokens).
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
                output_tokens: Some(100),
            },
        ],
        // Turn 2: tool call (100 more tokens → over budget).
        vec![
            StreamEvent::ToolUseStart {
                id: "call_2".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_2".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo bye"}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: Some(100),
            },
        ],
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    agent.conversation.token_budget.max_output_tokens = Some(150);
    let mut output = RecordingOutput::new();

    // Agent should stop due to budget, not error out.
    let _result = agent.run("run both", &mut output).await.unwrap();
    assert!(agent.conversation.token_budget.output_tokens_used >= 200);
    assert!(!agent.conversation.token_budget.has_budget());
}

// -------------------------------------------------------------------
// Input token tracking
// -------------------------------------------------------------------

#[test]
fn token_budget_tracks_input_tokens() {
    let mut budget = TokenBudget::default();
    assert_eq!(budget.input_tokens_used, 0);

    budget.record_input(500);
    assert_eq!(budget.input_tokens_used, 500);

    budget.record_input(300);
    assert_eq!(budget.input_tokens_used, 800);
}

#[test]
fn token_budget_reset_clears_input_tokens() {
    let mut budget = TokenBudget::default();
    budget.record_input(1000);
    budget.record(200).unwrap();
    budget.reset();
    assert_eq!(budget.input_tokens_used, 0);
    assert_eq!(budget.output_tokens_used, 0);
    assert_eq!(budget.llm_calls, 0);
}

// -------------------------------------------------------------------
// ToolMode enum
// -------------------------------------------------------------------

// -----------------------------------------------------------------------
// File sending tests
// -----------------------------------------------------------------------

/// A mock tool that returns a ToolOutput with attached files.
struct MockFileTool;

#[async_trait::async_trait]
impl crate::tool::Tool for MockFileTool {
    fn name(&self) -> &str {
        "send_test_file"
    }
    fn description(&self) -> &str {
        "Returns a file"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }
    async fn run(
        &self,
        _input: &serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> Result<ToolOutput> {
        Ok(ToolOutput::success("Here is your file.")
            .with_file("/tmp/test_report.pdf")
            .with_file("/tmp/data.csv"))
    }
}

/// A skill that provides only the MockFileTool.
struct MockFileSkill {
    tools: Vec<Arc<dyn crate::tool::Tool>>,
}

impl MockFileSkill {
    fn new() -> Self {
        Self {
            tools: vec![Arc::new(MockFileTool)],
        }
    }
}

#[async_trait::async_trait]
impl Skill for MockFileSkill {
    fn name(&self) -> &str {
        "mock_file_skill"
    }
    fn tools(&self) -> &[Arc<dyn crate::tool::Tool>] {
        &self.tools
    }
}

#[tokio::test]
async fn tool_output_files_dispatched_via_send_file() {
    // LLM calls send_test_file, then responds with text.
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_f1".into(),
                name: "send_test_file".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_f1".into(),
                name: "send_test_file".into(),
                input: serde_json::json!({}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("Files sent.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(MockFileSkill::new())];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    let result = agent.run("send me a file", &mut output).await.unwrap();
    assert_eq!(result, "Files sent.");

    // Verify that send_file was called for both attached files.
    let sent = output.sent_files();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], std::path::Path::new("/tmp/test_report.pdf"));
    assert_eq!(sent[1], std::path::Path::new("/tmp/data.csv"));
}

#[tokio::test]
async fn tool_output_no_files_means_no_send_file() {
    // Normal tool call without files — send_file should not be called.
    let llm = MockLlm::new(vec![
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
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("Done.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    agent.run("echo hello", &mut output).await.unwrap();

    // No files should have been sent.
    assert!(output.sent_files().is_empty());
}

#[test]
fn tool_mode_execute_vs_observe() {
    assert_ne!(crate::llm::ToolMode::Execute, crate::llm::ToolMode::Observe);
    // Copy semantics work.
    let mode = crate::llm::ToolMode::Observe;
    let copied = mode;
    assert_eq!(mode, copied);
}

// -----------------------------------------------------------------------
// CompactionConfig unit tests
// -----------------------------------------------------------------------

#[test]
fn compaction_config_default_values() {
    let config = CompactionConfig::default();
    assert_eq!(config.context_window, 200_000);
    assert!((config.threshold_ratio - 0.50).abs() < f64::EPSILON);
    assert_eq!(config.protect_head, 3);
    assert_eq!(config.protect_tail_tokens, 20_000);
    assert_eq!(config.summary_min_tokens, 2_000);
    assert_eq!(config.summary_max_tokens, 12_000);
    assert!((config.summary_target_ratio - 0.20).abs() < f64::EPSILON);
}

#[test]
fn compaction_config_threshold_calculation() {
    let config = CompactionConfig::default();
    // 200_000 * 0.50 = 100_000
    assert_eq!(config.threshold(), 100_000);
}

#[test]
fn compaction_config_threshold_with_custom_ratio() {
    let config = CompactionConfig {
        context_window: 128_000,
        threshold_ratio: 0.75,
        ..CompactionConfig::default()
    };
    // 128_000 * 0.75 = 96_000
    assert_eq!(config.threshold(), 96_000);
}

// -----------------------------------------------------------------------
// Helper: build an agent with manual message history for compaction tests.
// -----------------------------------------------------------------------

/// Build an agent with pre-loaded messages and a compaction config.
/// The `llm_responses` are the responses the MockLlm will return (e.g.
/// for the summarisation call during compact()).
fn make_agent_with_history(
    messages: Vec<Message>,
    llm_responses: Vec<Vec<StreamEvent>>,
    compaction: CompactionConfig,
) -> (Agent, RecordingOutput) {
    let llm = MockLlm::new(llm_responses);
    let settings = AgentSettings {
        api_key: "test".into(),
        compaction,
        ..Default::default()
    };
    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    agent.conversation.messages = messages;
    (agent, RecordingOutput::new())
}

// -----------------------------------------------------------------------
// Context compaction tests — five-phase Hermes-style compressor
// -----------------------------------------------------------------------

#[tokio::test]
async fn compact_on_empty_history_is_noop() {
    // No LLM responses queued — would panic if called.
    let (mut agent, mut output) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    agent.compact(&mut output).await.unwrap();
    assert!(agent.conversation.messages.is_empty());
}

#[tokio::test]
async fn compact_short_history_skips_when_no_middle() {
    // With protect_head=3 and only 3 messages, there's nothing to
    // summarise.  compact() should be a no-op (no LLM call).
    let messages = vec![
        Message::user("hello"),
        Message::assistant(vec![ContentBlock::Text { text: "hi!".into() }]),
        Message::user("how are you?"),
    ];
    let config = CompactionConfig {
        protect_head: 3,
        protect_tail_tokens: 0,
        ..CompactionConfig::default()
    };
    let (mut agent, mut output) =
        make_agent_with_history(messages.clone(), vec![], config);

    agent.compact(&mut output).await.unwrap();
    // All 3 messages preserved — no compaction needed.
    assert_eq!(agent.conversation.messages.len(), 3);
}

#[tokio::test]
async fn compact_preserves_head_and_tail() {
    // Build a conversation with 10 messages.  protect_head=2,
    // protect_tail_tokens=large enough to cover last 2 messages.
    // The middle 6 messages should be summarised.
    let mut messages = Vec::new();
    for i in 0..5 {
        messages.push(Message::user(&format!("User message {i}")));
        messages.push(Message::assistant(vec![ContentBlock::Text {
            text: format!("Assistant response {i}"),
        }]));
    }
    assert_eq!(messages.len(), 10);

    let config = CompactionConfig {
        protect_head: 2,
        // Each message is ~5 tokens.  Protect last 2 messages (~10 tokens).
        protect_tail_tokens: 15,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta(
            "## Goal\nTest conversation\n## Progress\nMessages exchanged.".into(),
        ),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages.clone(), vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // Head: first 2 messages preserved verbatim.
    assert_eq!(agent.conversation.messages[0].role, Role::User);
    match &agent.conversation.messages[0].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "User message 0"),
        other => panic!("expected Text, got: {other:?}"),
    }
    assert_eq!(agent.conversation.messages[1].role, Role::Assistant);
    match &agent.conversation.messages[1].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Assistant response 0"),
        other => panic!("expected Text, got: {other:?}"),
    }

    // Summary should be present somewhere after head.
    let summary_idx = agent.conversation.messages.iter().position(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
    });
    assert!(summary_idx.is_some(), "summary message should exist");

    // Tail: last 2 original messages preserved verbatim.
    let last = &agent.conversation.messages[agent.conversation.messages.len() - 1];
    match &last.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Assistant response 4"),
        other => panic!("expected Text, got: {other:?}"),
    }
    let second_last = &agent.conversation.messages[agent.conversation.messages.len() - 2];
    match &second_last.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "User message 4"),
        other => panic!("expected Text, got: {other:?}"),
    }
}

#[tokio::test]
async fn compact_prunes_tool_outputs_in_middle() {
    // Messages: head(user, assistant) + middle(assistant-with-tool, tool-result) + tail(user, assistant)
    let messages = vec![
        // Head
        Message::user("start"),
        Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
        // Middle — tool call + large result
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls -la"}),
        }]),
        Message::tool_result(
            "call_1",
            "drwxr-xr-x 15 user user 4096 Mar 30 file1.txt\n-rw-r--r-- 1 user user 12345 Mar 30 file2.txt\n...(many more lines)...",
            false,
        ),
        // More middle
        Message::user("what about the other directory?"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "call_2".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls /other"}),
        }]),
        Message::tool_result(
            "call_2",
            "big output from other directory listing here",
            false,
        ),
        // Tail
        Message::user("thanks, now summarise"),
        Message::assistant(vec![ContentBlock::Text {
            text: "Here's your summary.".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 100, // enough for last 2 messages
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta(
            "## Goal\nFile listing\n## Progress\nListed directories.".into(),
        ),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // The summary should exist and tool outputs in the middle should
    // have been pruned (replaced with placeholder) before summarisation.
    let has_summary = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text.contains("[Context Summary]")),
        )
    });
    assert!(has_summary, "should contain a context summary");

    // Original large tool outputs should NOT be in the final messages.
    let has_big_output = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("many more lines")))
    });
    assert!(
        !has_big_output,
        "large tool outputs in middle should be pruned or summarised away"
    );
}

#[tokio::test]
async fn compact_fixes_orphaned_tool_pairs() {
    // Set up a situation where compaction splits a tool_use/tool_result pair:
    // - Head contains an assistant message with tool_use
    // - The matching tool_result is in the middle (gets summarised away)
    // After compaction, the orphaned tool_use should get a synthetic result.
    let messages = vec![
        // Head
        Message::user("start"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "orphan_call".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "echo test"}),
        }]),
        // Middle — the tool result for orphan_call, plus more conversation
        Message::tool_result("orphan_call", "test output", false),
        Message::user("continue"),
        Message::assistant(vec![ContentBlock::Text {
            text: "continuing...".into(),
        }]),
        Message::user("more stuff"),
        Message::assistant(vec![ContentBlock::Text {
            text: "more responses".into(),
        }]),
        // Tail
        Message::user("final question"),
        Message::assistant(vec![ContentBlock::Text {
            text: "final answer".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 100,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta("## Goal\nTesting\n## Progress\nRan commands.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // The head still has the tool_use for "orphan_call".
    let has_tool_use = agent.conversation.messages[1]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "orphan_call"));
    assert!(has_tool_use, "head should still contain the tool_use");

    // There should be a synthetic tool_result matching "orphan_call"
    // (since the real one was in the middle and got summarised away).
    let has_matching_result = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "orphan_call")
        })
    });
    assert!(
        has_matching_result,
        "should have a synthetic tool_result for the orphaned tool_use"
    );
}

#[tokio::test]
async fn compact_structured_summary_prompt() {
    // Verify that the LLM receives a structured prompt asking for
    // Goal/Progress/Decisions/Files/Next Steps sections.
    // We check this indirectly: the summary returned by the LLM
    // gets inserted as a [Context Summary] message.
    let messages = vec![
        Message::user("msg 0"),
        Message::assistant(vec![ContentBlock::Text {
            text: "resp 0".into(),
        }]),
        Message::user("msg 1"),
        Message::assistant(vec![ContentBlock::Text {
            text: "resp 1".into(),
        }]),
        Message::user("msg 2"),
        Message::assistant(vec![ContentBlock::Text {
            text: "resp 2".into(),
        }]),
        Message::user("msg 3"),
        Message::assistant(vec![ContentBlock::Text {
            text: "resp 3".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 15,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta("## Goal\nUser was testing.\n## Progress\nMultiple exchanges.\n## Key Decisions\nNone.\n## Files Modified\nNone.\n## Next Steps\nContinue.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // Find the summary message.
    let summary_msg = agent.conversation.messages.iter().find(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
    }).expect("should have a summary message");

    match &summary_msg.content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("Goal"), "summary should contain Goal section");
            assert!(
                text.contains("Progress"),
                "summary should contain Progress section"
            );
        }
        other => panic!("expected Text, got: {other:?}"),
    }
}

#[tokio::test]
async fn compact_resets_token_budget() {
    let messages = vec![
        Message::user("hello"),
        Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
        Message::user("more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "more".into(),
        }]),
        Message::user("even more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "even more".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 15,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta("Summary.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: Some(10),
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.conversation.token_budget.record(50).unwrap();
    assert_eq!(agent.conversation.token_budget.output_tokens_used, 50);

    agent.compact(&mut output).await.unwrap();

    assert_eq!(agent.conversation.token_budget.output_tokens_used, 0);
    assert_eq!(agent.conversation.token_budget.input_tokens_used, 0);
    assert_eq!(agent.conversation.token_budget.llm_calls, 0);
}

#[tokio::test]
async fn compact_iterative_merges_with_previous_summary() {
    // Simulate a second compaction: the head already contains a
    // [Context Summary] from a previous compaction.  The new compact
    // should produce an updated summary that merges old + new.
    let messages = vec![
        // Previous summary (from first compaction).
        Message::user(
            "[Context Summary]\n\n## Goal\nOriginal goal.\n## Progress\nStep 1 done.",
        ),
        // New conversation since last compaction.
        Message::assistant(vec![ContentBlock::Text {
            text: "continuing work".into(),
        }]),
        Message::user("do step 2"),
        Message::assistant(vec![ContentBlock::Text {
            text: "step 2 done".into(),
        }]),
        Message::user("do step 3"),
        Message::assistant(vec![ContentBlock::Text {
            text: "step 3 done".into(),
        }]),
        // Tail
        Message::user("what's next?"),
        Message::assistant(vec![ContentBlock::Text {
            text: "step 4".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 15,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta(
            "## Goal\nOriginal goal.\n## Progress\nSteps 1-3 done.\n## Next Steps\nStep 4."
                .into(),
        ),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // Should have a merged summary.
    let summary_msg = agent.conversation.messages.iter().find(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
    }).expect("should have a summary message");

    match &summary_msg.content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("Steps 1-3"),
                "summary should merge old + new progress"
            );
        }
        other => panic!("expected Text, got: {other:?}"),
    }

    // Should NOT have two [Context Summary] messages.
    let summary_count = agent.conversation.messages.iter().filter(|m| {
        m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Context Summary]")))
    }).count();
    assert_eq!(
        summary_count, 1,
        "should have exactly one summary after iterative compaction"
    );
}

#[tokio::test]
async fn compact_empty_summary_keeps_original_history() {
    // If the LLM returns an empty summary, keep the original history.
    let messages = vec![
        Message::user("hello"),
        Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
        Message::user("more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "more".into(),
        }]),
        Message::user("even more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "even more".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 15,
        ..CompactionConfig::default()
    };

    // LLM returns empty text.
    let summary_response = vec![StreamEvent::MessageComplete {
        stop_reason: StopReason::EndTurn,
        output_tokens: None,
    }];

    let original_len = messages.len();
    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();
    // Original history should be preserved (though tool outputs may be pruned).
    assert_eq!(agent.conversation.messages.len(), original_len);
}

#[tokio::test]
async fn compact_tail_protection_by_token_budget() {
    // Verify that tail protection is based on token budget, not message count.
    // Create messages with very different token sizes — the tail should protect
    // the last messages that fit within the token budget.
    let messages = vec![
        Message::user("hi"),                // ~5 tokens
        Message::assistant(vec![ContentBlock::Text { text: "hello".into() }]), // ~5 tokens
        Message::user("middle msg"),        // ~6 tokens
        Message::assistant(vec![ContentBlock::Text { text: "middle resp".into() }]), // ~6 tokens
        // These two are large — should be in the tail if budget is generous.
        Message::user("a very long user message with many words to take up lots of token budget space in the estimate"),
        Message::assistant(vec![ContentBlock::Text {
            text: "a very long assistant response with many words to take up lots of token budget space in the estimate".into(),
        }]),
    ];

    let config = CompactionConfig {
        protect_head: 2,
        // Budget large enough for the last 2 big messages (~40+ tokens),
        // but NOT for all 4 non-head messages.
        protect_tail_tokens: 50,
        ..CompactionConfig::default()
    };

    let summary_response = vec![
        StreamEvent::TextDelta("Middle section summary.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let (mut agent, mut output) =
        make_agent_with_history(messages, vec![summary_response], config);

    agent.compact(&mut output).await.unwrap();

    // Head (2 messages) + summary (1) + tail (2 big messages) = 5.
    // The middle 2 messages got summarised.
    let last_text = agent.conversation.messages.last().unwrap();
    match &last_text.content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("very long assistant response"),
                "tail should preserve the last large messages"
            );
        }
        other => panic!("expected Text, got: {other:?}"),
    }
}

#[tokio::test]
async fn auto_compaction_triggers_on_threshold() {
    // Set up a very low compaction threshold so that after turn 1 builds
    // up history, the offline token estimate exceeds it on turn 2.
    let llm = MockLlm::new(vec![
        // Turn 1: normal response.
        vec![
            StreamEvent::TextDelta("First response.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: Some(20),
            },
        ],
        // Auto-compaction summary.
        vec![
            StreamEvent::TextDelta("Summary of turn 1.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: Some(5),
            },
        ],
        // Turn 2: normal response after compaction.
        vec![
            StreamEvent::TextDelta("Second response.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: Some(20),
            },
        ],
    ]);

    let settings = AgentSettings {
        api_key: "test".into(),
        compaction: CompactionConfig {
            context_window: 20, // very low
            threshold_ratio: 0.50,
            protect_head: 1,
            protect_tail_tokens: 0,
            ..CompactionConfig::default()
        },
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    let mut output = RecordingOutput::new();

    // First turn.
    agent.run("first message", &mut output).await.unwrap();
    assert_eq!(agent.conversation.messages.len(), 2);

    // Second turn — triggers auto-compact.
    let result = agent.run("second message", &mut output).await.unwrap();
    assert_eq!(result, "Second response.");
}

// -----------------------------------------------------------------------
// Compact rotation tests — pre-compaction history preservation.
// -----------------------------------------------------------------------

#[tokio::test]
async fn compact_rotates_pre_compaction_history() {
    // When a chat history backend is attached, compact() should save
    // and rotate the pre-compaction messages before summarising.
    let messages = vec![
        Message::user("hello"),
        Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
        Message::user("more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "more response".into(),
        }]),
        Message::user("even more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "even more response".into(),
        }]),
    ];

    let summary_response = vec![
        StreamEvent::TextDelta("Summary of conversation.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let config = CompactionConfig {
        protect_head: 2,
        protect_tail_tokens: 100,
        ..CompactionConfig::default()
    };

    let (mut agent, mut output) =
        make_agent_with_history(messages.clone(), vec![summary_response], config);

    // Attach a disk chat history so we can verify the rotation.
    let dir = std::env::temp_dir().join(format!(
        "dyson_compact_rotate_test_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = crate::chat_history::DiskChatHistory::new(dir.clone()).unwrap();
    let store: Arc<dyn crate::chat_history::ChatHistory> = Arc::new(store);

    agent.set_chat_history(store.clone(), "test_chat".to_string());

    // Run compact.
    agent.compact(&mut output).await.unwrap();

    // Current chat file should be empty (it was rotated).
    let current = store.load("test_chat").unwrap();
    assert!(current.is_empty(), "current chat should be empty after rotation");

    // A rotated file should exist with the pre-compaction messages.
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("test_chat.") && name.ends_with(".json")
        })
        .collect();
    assert_eq!(files.len(), 1, "should have exactly one rotated file");

    // The rotated file should contain the original 6 messages.
    let rotated_content = std::fs::read_to_string(files[0].path()).unwrap();
    let rotated_msgs: Vec<Message> = serde_json::from_str(&rotated_content).unwrap();
    assert_eq!(rotated_msgs.len(), 6, "rotated file should have all pre-compaction messages");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_without_chat_history_does_not_rotate() {
    // When no chat history is attached, compact should work normally.
    let messages = vec![
        Message::user("hello"),
        Message::assistant(vec![ContentBlock::Text { text: "hi".into() }]),
        Message::user("more"),
        Message::assistant(vec![ContentBlock::Text {
            text: "more".into(),
        }]),
    ];

    let summary_response = vec![
        StreamEvent::TextDelta("Summary.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ];

    let config = CompactionConfig {
        protect_head: 1,
        protect_tail_tokens: 0,
        ..CompactionConfig::default()
    };

    let (mut agent, mut output) = make_agent_with_history(
        messages,
        vec![summary_response],
        config,
    );

    assert!(agent.history_backend.is_none());
    agent.compact(&mut output).await.unwrap();

    // Head (1 msg) + summary + no tail = 2 messages.
    assert_eq!(agent.conversation.messages.len(), 2);
}

// -----------------------------------------------------------------------
// Quick response tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn quick_response_returns_text_without_tools() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Quick answer.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let history = vec![
        Message::user("What is 2+2?"),
        Message::assistant(vec![ContentBlock::Text {
            text: "4".into(),
        }]),
    ];

    let config = CompletionConfig {
        model: "test-model".into(),
        max_tokens: 4096,
        temperature: None,
        api_tool_injections: vec![],
    };

    let mut output = RecordingOutput::new();
    let result = quick_response(
        &llm,
        &history,
        "You are a helpful assistant.",
        "What about 3+3?",
        &config,
        &mut output,
    )
    .await
    .unwrap();

    assert_eq!(result, "Quick answer.");
    assert_eq!(output.text(), "Quick answer.");
}

#[tokio::test]
async fn quick_response_caps_max_tokens() {
    // Verify that quick_response uses min(config.max_tokens, 1024).
    use std::sync::Mutex as StdMutex;

    struct CapturingLlm {
        captured_max_tokens: StdMutex<Option<u32>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for CapturingLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            _tools: &[ToolDefinition],
            config: &CompletionConfig,
        ) -> Result<crate::llm::StreamResponse> {
            *self.captured_max_tokens.lock().unwrap() = Some(config.max_tokens);
            let events = vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: None,
                },
            ];
            Ok(crate::llm::StreamResponse {
                stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                tool_mode: crate::llm::ToolMode::Execute,
                input_tokens: None,
            })
        }
    }

    let llm = CapturingLlm {
        captured_max_tokens: StdMutex::new(None),
    };
    let config = CompletionConfig {
        model: "test".into(),
        max_tokens: 8192,
        temperature: None,
        api_tool_injections: vec![],
    };
    let mut output = RecordingOutput::new();

    quick_response(&llm, &[], "sys", "hi", &config, &mut output)
        .await
        .unwrap();

    assert_eq!(*llm.captured_max_tokens.lock().unwrap(), Some(1024));
}

#[tokio::test]
async fn quick_response_sends_no_tools() {
    use std::sync::Mutex as StdMutex;

    struct ToolCapturingLlm {
        captured_tools: StdMutex<Option<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for ToolCapturingLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<crate::llm::StreamResponse> {
            *self.captured_tools.lock().unwrap() = Some(tools.len());
            let events = vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    output_tokens: None,
                },
            ];
            Ok(crate::llm::StreamResponse {
                stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
                tool_mode: crate::llm::ToolMode::Execute,
                input_tokens: None,
            })
        }
    }

    let llm = ToolCapturingLlm {
        captured_tools: StdMutex::new(None),
    };
    let config = CompletionConfig {
        model: "test".into(),
        max_tokens: 1024,
        temperature: None,
        api_tool_injections: vec![],
    };
    let mut output = RecordingOutput::new();

    quick_response(&llm, &[], "sys", "hi", &config, &mut output)
        .await
        .unwrap();

    assert_eq!(*llm.captured_tools.lock().unwrap(), Some(0));
}

// -----------------------------------------------------------------------
// Integration tests for the tool calling pipeline.
// -----------------------------------------------------------------------

mod test_tool_calling_integration {
    use super::*;
    use super::dependency_analyzer::{DependencyAnalyzer, ExecutionPhase};
    use super::result_formatter::ResultFormatter;
    use super::tool_limiter::ToolLimiter;

    #[test]
    fn full_pipeline_single_call() {
        // Verify: limits check -> (would execute) -> format.
        // We test the pipeline components in isolation since the full
        // agent.run() requires an LLM client + async runtime.

        let mut limiter = ToolLimiter::default();
        let formatter = ResultFormatter::default();

        let call = ToolCall::new("bash", serde_json::json!({"command": "echo hello"}));

        // 1. Limiter allows the call.
        assert!(limiter.check(&call.name).is_ok());

        // 2. Dependency analysis: single call → one parallel phase.
        let phases = DependencyAnalyzer::analyze(&[&call]);
        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], ExecutionPhase::Parallel(_)));

        // 3. Format the result.
        let output = ToolOutput::success("hello");
        let formatted = formatter.format(&call, &output, std::time::Duration::from_millis(10));
        assert!(formatted.summary.contains("10ms"));
        assert!(!formatted.to_llm_message().is_empty());
    }

    #[test]
    fn respects_dependency_ordering() {
        // write then read = sequential phases.
        let calls = [
            ToolCall::new("file_write", serde_json::json!({"path": "out.txt"})),
            ToolCall::new("file_read", serde_json::json!({"path": "out.txt"})),
        ];
        let refs: Vec<&ToolCall> = calls.iter().collect();
        let phases = DependencyAnalyzer::analyze(&refs);
        assert!(
            phases.len() >= 2,
            "expected at least 2 phases, got {}",
            phases.len()
        );
    }

    #[test]
    fn applies_limits_in_pipeline() {
        // Hit the per-turn limit → error without executing.
        // Use a limiter with no cooldown by checking rapidly (the
        // default cooldown is 1s, but we're checking per-turn limits,
        // which are separate from cooldown).
        let mut limiter = ToolLimiter::default();

        // The first call succeeds.
        assert!(limiter.check("bash").is_ok());

        // The per-turn limit is 50; after 1 successful call above,
        // the limiter tracks this tool. A second immediate call fails
        // due to cooldown — but that still proves limits work in the
        // pipeline.
        let result = limiter.check("bash");
        assert!(
            result.is_err(),
            "second immediate call should be rate-limited"
        );
    }

    #[test]
    fn pop_last_message_removes_last() {
        let messages = vec![Message::user("hello"), Message::user("world")];
        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };
        let llm = MockLlm::new(vec![]);
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
        agent.set_messages(messages.clone());

        let popped = agent.pop_last_message();
        assert_eq!(
            popped.unwrap().content[0],
            ContentBlock::Text {
                text: "world".into()
            }
        );
        assert_eq!(agent.messages().len(), 1);
    }

    #[test]
    fn pop_last_message_on_empty_returns_none() {
        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };
        let llm = MockLlm::new(vec![]);
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();

        assert!(agent.pop_last_message().is_none());
    }

    #[test]
    fn strip_images_replaces_image_blocks_with_placeholder() {
        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };
        let llm = MockLlm::new(vec![]);
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();

        agent.set_messages(vec![
            Message::user_multimodal(vec![
                ContentBlock::Text {
                    text: "look at this".into(),
                },
                ContentBlock::Image {
                    data: "base64data".into(),
                    media_type: "image/jpeg".into(),
                },
            ]),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I see a cat".into(),
                }],
            },
            Message::user("thanks"),
        ]);

        agent.strip_images();

        // The image block should be replaced with "[image]" text.
        let first_msg = &agent.messages()[0];
        assert_eq!(first_msg.content.len(), 2);
        assert_eq!(
            first_msg.content[1],
            ContentBlock::Text {
                text: "[image]".into()
            },
        );

        // Text-only messages should be untouched.
        assert_eq!(
            agent.messages()[2].content[0],
            ContentBlock::Text {
                text: "thanks".into()
            }
        );
    }

    #[test]
    fn strip_images_noop_when_no_images() {
        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };
        let llm = MockLlm::new(vec![]);
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = Agent::new(rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();

        agent.set_messages(vec![
            Message::user("hello"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            },
        ]);

        agent.strip_images();

        assert_eq!(agent.messages().len(), 2);
        assert_eq!(
            agent.messages()[0].content[0],
            ContentBlock::Text {
                text: "hello".into()
            }
        );
    }
}

// -----------------------------------------------------------------------
// Compaction helper unit tests (Step 3)
// -----------------------------------------------------------------------

#[test]
fn head_boundary_clamps_to_message_count() {
    // protect_head > number of messages → clamp to len.
    let (agent, _) = make_agent_with_history(
        vec![Message::user("only one")],
        vec![],
        CompactionConfig {
            protect_head: 100,
            ..CompactionConfig::default()
        },
    );
    let config = agent.compaction_config;
    assert_eq!(agent.head_boundary(&config), 1);
}

#[test]
fn head_boundary_normal() {
    let msgs = vec![
        Message::user("a"),
        Message::assistant(vec![ContentBlock::Text { text: "b".into() }]),
        Message::user("c"),
        Message::assistant(vec![ContentBlock::Text { text: "d".into() }]),
    ];
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig {
            protect_head: 2,
            ..CompactionConfig::default()
        },
    );
    let config = agent.compaction_config;
    assert_eq!(agent.head_boundary(&config), 2);
}

#[test]
fn tail_boundary_all_fit() {
    // All non-head messages fit within the tail budget.
    let msgs = vec![
        Message::user("head"),
        Message::user("a"),
        Message::user("b"),
    ];
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig {
            protect_head: 1,
            protect_tail_tokens: 100_000, // huge budget
            ..CompactionConfig::default()
        },
    );
    let config = agent.compaction_config;
    // All non-head fit → tail_start == head_end.
    assert_eq!(agent.tail_boundary(&config), agent.head_boundary(&config));
}

#[test]
fn tail_boundary_partial() {
    // Budget exhausted mid-conversation.
    let msgs: Vec<Message> = (0..10)
        .map(|i| Message::user(&format!("message {i} with some words")))
        .collect();
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig {
            protect_head: 1,
            protect_tail_tokens: 10, // very small budget
            ..CompactionConfig::default()
        },
    );
    let config = agent.compaction_config;
    let tail_start = agent.tail_boundary(&config);
    // tail_start should be after head_end and before the end.
    assert!(tail_start > agent.head_boundary(&config));
    assert!(tail_start < agent.conversation.messages.len());
}

#[test]
fn tail_boundary_empty() {
    // No messages at all.
    let (agent, _) = make_agent_with_history(
        vec![],
        vec![],
        CompactionConfig {
            protect_head: 0,
            protect_tail_tokens: 1000,
            ..CompactionConfig::default()
        },
    );
    let config = agent.compaction_config;
    assert_eq!(agent.tail_boundary(&config), 0);
}

#[test]
fn find_existing_summary_found() {
    let msgs = vec![
        Message::user("[Context Summary]\n\nPrevious summary content."),
        Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
        Message::user("continue"),
    ];
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    let summary = agent.find_existing_summary(2);
    assert!(summary.is_some());
    assert!(summary.unwrap().contains("Previous summary content"));
}

#[test]
fn find_existing_summary_not_found() {
    let msgs = vec![
        Message::user("no summary here"),
        Message::assistant(vec![ContentBlock::Text { text: "ok".into() }]),
    ];
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    assert!(agent.find_existing_summary(2).is_none());
}

#[test]
fn fix_orphaned_uses_inserts_synthetic() {
    // ToolUse without matching ToolResult.
    let msgs = vec![
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "orphan_1".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]),
        Message::user("continue"),
    ];
    let (mut agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    agent.fix_orphaned_tool_pairs();
    // Should now have a synthetic ToolResult for orphan_1.
    let has_result = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "orphan_1")
        })
    });
    assert!(has_result, "should insert synthetic result for orphaned tool_use");
}

#[test]
fn fix_orphaned_results_removed() {
    // ToolResult without matching ToolUse.
    let msgs = vec![
        Message::user("start"),
        Message::tool_result("missing_call", "orphaned result", false),
        Message::user("continue"),
    ];
    let (mut agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    agent.fix_orphaned_tool_pairs();
    // The orphaned result should be removed.
    let has_orphan = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "missing_call")
        })
    });
    assert!(!has_orphan, "orphaned tool_result should be removed");
}

#[test]
fn fix_orphaned_mixed() {
    // Both orphaned uses and orphaned results simultaneously.
    let msgs = vec![
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "use_no_result".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
        }]),
        Message::tool_result("result_no_use", "orphaned", false),
        Message::user("end"),
    ];
    let (mut agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    agent.fix_orphaned_tool_pairs();

    // Synthetic result for "use_no_result" should exist.
    let has_synthetic = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "use_no_result")
        })
    });
    assert!(has_synthetic);

    // Orphaned result for "result_no_use" should be removed.
    let has_orphan = agent.conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "result_no_use")
        })
    });
    assert!(!has_orphan);
}

#[test]
fn estimate_context_tokens_basic() {
    let msgs = vec![
        Message::user("hello world"),
        Message::assistant(vec![ContentBlock::Text { text: "hi there friend".into() }]),
    ];
    let (agent, _) = make_agent_with_history(
        msgs,
        vec![],
        CompactionConfig::default(),
    );
    let tokens = agent.estimate_context_tokens("system prompt here");
    // system: 3 words + messages: ~2 + ~3 + framing → should be > 0
    assert!(tokens > 0);
    // Should include system prompt words.
    assert!(tokens >= 3);
}

#[test]
fn build_compaction_prompt_with_previous() {
    let (agent, _) = make_agent_with_history(
        vec![Message::user("test")],
        vec![],
        CompactionConfig::default(),
    );
    let prompt = agent.build_compaction_prompt(Some("Previous summary text."));
    assert!(prompt.contains("Previous context summary"));
    assert!(prompt.contains("Previous summary text."));
    assert!(prompt.contains("## Goal"));
}

#[test]
fn build_compaction_prompt_without_previous() {
    let (agent, _) = make_agent_with_history(
        vec![Message::user("test")],
        vec![],
        CompactionConfig::default(),
    );
    let prompt = agent.build_compaction_prompt(None);
    assert!(prompt.contains("## Goal"));
    assert!(!prompt.contains("Previous context summary"));
}

// -----------------------------------------------------------------------
// is_retryable tests (Step 4)
// -----------------------------------------------------------------------

#[test]
fn is_retryable_rate_limit() {
    assert!(Agent::is_retryable(&DysonError::LlmRateLimit("rate limit exceeded".into())));
    assert!(Agent::is_retryable(&DysonError::LlmRateLimit("HTTP 429 Too Many Requests".into())));
}

#[test]
fn is_retryable_overloaded() {
    assert!(Agent::is_retryable(&DysonError::LlmOverloaded("server overloaded".into())));
    assert!(Agent::is_retryable(&DysonError::LlmOverloaded("HTTP 529".into())));
    assert!(Agent::is_retryable(&DysonError::LlmOverloaded("HTTP 502 Bad Gateway".into())));
    assert!(Agent::is_retryable(&DysonError::LlmOverloaded("HTTP 503 Service Unavailable".into())));
}

#[tokio::test]
async fn is_retryable_http_error() {
    // DysonError::Http is always retryable. Trigger a real reqwest error
    // by trying to connect to a port that's not listening.
    let err = reqwest::Client::new()
        .get("http://127.0.0.1:1")
        .send()
        .await
        .unwrap_err();
    assert!(Agent::is_retryable(&DysonError::Http(err)));
}

#[test]
fn is_retryable_other() {
    assert!(!Agent::is_retryable(&DysonError::Config("bad config".into())));
    assert!(!Agent::is_retryable(&DysonError::tool("bash", "command failed")));
    assert!(!Agent::is_retryable(&DysonError::Cancelled));
}

// -----------------------------------------------------------------------
// Advisor tests
// -----------------------------------------------------------------------

#[test]
fn generic_advisor_inherits_parent_tools() {
    // Create a generic advisor and verify it registers an "advisor" tool
    // that has access to the parent agent's tools.
    let llm = MockLlm::new(vec![]);
    let advisor_llm = MockLlm::new(vec![]);

    let settings = AgentSettings {
        api_key: "test".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);

    // Count the tools from skills (before advisor).
    let skill_tool_count: usize = skills.iter().map(|s| s.tools().len()).sum();
    assert!(skill_tool_count > 0, "skills should provide at least one tool");

    let advisor: Box<dyn crate::advisor::Advisor> = Box::new(
        crate::advisor::generic::GenericAdvisor::new(
            "test-advisor-model".to_string(),
            crate::config::LlmProvider::OpenAi,
            rate_limiter::RateLimitedHandle::unlimited(Box::new(advisor_llm)),
        ),
    );

    let agent = Agent::new(
        rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        sandbox,
        skills,
        &settings,
        None,
        0,
        None,
        Some(advisor),
    )
    .unwrap();

    // The agent should have all the original tools PLUS the advisor tool.
    assert_eq!(
        agent.tool_registry.tools.len(),
        skill_tool_count + 1,
        "advisor tool should be registered alongside skill tools"
    );
    assert!(
        agent.tool_registry.tools.contains_key("advisor"),
        "tool registry should contain the 'advisor' tool"
    );

    // API tool injections should be empty for the generic path.
    assert!(
        agent.config.api_tool_injections.is_empty(),
        "generic advisor should not inject API tools"
    );
}

#[test]
fn generic_advisor_shares_sandbox_with_parent() {
    // Verify that the generic advisor's child agent gets the exact same
    // sandbox instance (Arc identity) as the parent agent.
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
    let advisor_llm = MockLlm::new(vec![]);

    let mut advisor = crate::advisor::generic::GenericAdvisor::new(
        "test-advisor".to_string(),
        crate::config::LlmProvider::OpenAi,
        rate_limiter::RateLimitedHandle::unlimited(Box::new(advisor_llm)),
    );

    // Simulate what Agent::new() does: collect parent tools, call bind().
    let parent_tools: Vec<Arc<dyn Tool>> = vec![];
    crate::advisor::Advisor::bind(&mut advisor, Arc::clone(&sandbox), None, parent_tools);

    // The advisor's sandbox should be the same Arc instance.
    let advisor_sandbox = advisor.sandbox().expect("sandbox should be set after bind()");
    assert!(
        Arc::ptr_eq(&sandbox, advisor_sandbox),
        "advisor sandbox must be the same Arc instance as the parent's"
    );
}

#[test]
fn native_anthropic_advisor_injects_api_tool() {
    // When the executor is Anthropic, the advisor should inject an
    // advisor_20260301 tool entry and NOT register any Dyson-side tools.
    let llm = MockLlm::new(vec![]);

    let settings = AgentSettings {
        api_key: "test".into(),
        provider: crate::config::LlmProvider::Anthropic,
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new(None, None, None))];
    let skill_tool_count: usize = skills.iter().map(|s| s.tools().len()).sum();
    let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);

    let advisor_client = rate_limiter::RateLimitedHandle::unlimited(
        Box::new(MockLlm::new(vec![])) as Box<dyn LlmClient>,
    );
    let advisor = crate::advisor::create_advisor(
        &crate::config::LlmProvider::Anthropic,
        &crate::config::LlmProvider::Anthropic,
        "claude-opus-4-6",
        advisor_client,
    );

    let agent = Agent::new(
        rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        sandbox,
        skills,
        &settings,
        None,
        0,
        None,
        Some(advisor),
    )
    .unwrap();

    // No extra Dyson-side tools — native advisor is API-level.
    assert_eq!(
        agent.tool_registry.tools.len(),
        skill_tool_count,
        "native advisor should not add Dyson-side tools"
    );

    // Should have one API tool injection.
    assert_eq!(agent.config.api_tool_injections.len(), 1);
    let injection = &agent.config.api_tool_injections[0];
    assert_eq!(injection["type"], "advisor_20260301");
    assert_eq!(injection["name"], "advisor");
    assert_eq!(injection["model"], "claude-opus-4-6");
    assert_eq!(injection["max_uses"], 3);
}
