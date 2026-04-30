use super::*;
use crate::llm::stream::{StopReason, StreamEvent};

// -----------------------------------------------------------------------
// CaptureOutput tests
// -----------------------------------------------------------------------

#[test]
fn capture_output_collects_text_deltas() {
    let mut output = CaptureOutput::new();
    output.text_delta("Hello, ").unwrap();
    output.text_delta("world!").unwrap();
    assert_eq!(output.text(), "Hello, world!");
}

#[test]
fn capture_output_starts_empty() {
    let output = CaptureOutput::new();
    assert_eq!(output.text(), "");
}

#[test]
fn capture_output_handles_tool_events() {
    let mut output = CaptureOutput::new();
    output.tool_use_start("id_1", "bash").unwrap();
    output.tool_use_complete().unwrap();
    output.tool_result(&ToolOutput::success("result")).unwrap();
    // Tool events should not add to the captured text.
    assert_eq!(output.text(), "");
}

#[test]
fn capture_output_handles_errors() {
    let mut output = CaptureOutput::new();
    output.error(&DysonError::Llm("test error".into())).unwrap();
    // Errors are logged, not captured as text.
    assert_eq!(output.text(), "");
}

#[test]
fn capture_output_handles_flush() {
    let mut output = CaptureOutput::new();
    output.text_delta("text").unwrap();
    output.flush().unwrap();
    assert_eq!(output.text(), "text");
}

#[test]
fn capture_output_ignores_file_sends() {
    let mut output = CaptureOutput::new();
    output
        .send_file(std::path::Path::new("/tmp/test.pdf"))
        .unwrap();
    assert_eq!(output.text(), "");
}

// -----------------------------------------------------------------------
// CaptureOutput UI tee — backstop for the "empty box until done" bug.
// Without these, the LLM-boundary path could silently regress and the
// UI would go back to looking dead while a subagent runs.
// -----------------------------------------------------------------------

/// Build a `CaptureOutput` with a real `SubagentEventBus` wired and
/// return both halves so a test can assert on the broadcast frames.
fn capture_with_bus(parent_tool_id: &str)
    -> (CaptureOutput, tokio::sync::broadcast::Receiver<crate::controller::http::SseEvent>)
{
    let (tx, rx) = tokio::sync::broadcast::channel(64);
    let bus = crate::controller::http::SubagentEventBus::new(tx);
    let cap = CaptureOutput::new()
        .with_ui_sink(Some(bus), Some(parent_tool_id.to_string()));
    (cap, rx)
}

#[test]
fn capture_output_tees_inner_tool_start_with_parent_id() {
    use crate::controller::http::SseEvent;
    let (mut cap, mut rx) = capture_with_bus("parent_subagent");
    cap.tool_use_start("inner_1", "bash").unwrap();
    match rx.try_recv().unwrap() {
        SseEvent::ToolStart { id, name, parent_tool_id } => {
            assert_eq!(id, "inner_1");
            assert_eq!(name, "bash");
            assert_eq!(parent_tool_id.as_deref(), Some("parent_subagent"));
        }
        other => panic!("unexpected: {}", serde_json::to_string(&other).unwrap()),
    }
}

#[test]
fn capture_output_tees_tool_result_with_parent_and_inner_id() {
    use crate::controller::http::SseEvent;
    let (mut cap, mut rx) = capture_with_bus("parent_subagent");
    cap.tool_use_start("inner_1", "bash").unwrap();
    let _ = rx.try_recv(); // discard the ToolStart frame
    cap.tool_result(&ToolOutput::success("ok")).unwrap();
    match rx.try_recv().unwrap() {
        SseEvent::ToolResult { content, is_error, parent_tool_id, tool_use_id, .. } => {
            assert_eq!(content, "ok");
            assert!(!is_error);
            assert_eq!(parent_tool_id.as_deref(), Some("parent_subagent"));
            assert_eq!(tool_use_id.as_deref(), Some("inner_1"));
        }
        other => panic!("unexpected: {}", serde_json::to_string(&other).unwrap()),
    }
}

#[test]
fn capture_output_tee_is_silent_when_bus_is_unset() {
    // Default `CaptureOutput::new()` — no bus, no parent — must not
    // attempt to construct any SSE frames.  No assertion needed
    // beyond "doesn't panic" because the broadcast channel doesn't
    // exist; we just make sure the code path stays no-op.
    let mut cap = CaptureOutput::new();
    cap.tool_use_start("inner_1", "bash").unwrap();
    cap.tool_result(&ToolOutput::success("ok")).unwrap();
    cap.send_file(std::path::Path::new("/tmp/x")).unwrap();
    let art = crate::message::Artefact {
        id: String::new(),
        kind: crate::message::ArtefactKind::Other,
        title: "t".into(),
        content: "c".into(),
        mime_type: "text/plain".into(),
        metadata: None,
    };
    cap.send_artefact(&art).unwrap();
    // Artefacts are buffered for the parent regardless of the UI bus —
    // this is the LLM-side capture path, not the UI tee.
    assert_eq!(cap.take_artefacts().len(), 1);
}

#[test]
fn capture_output_tee_requires_both_bus_and_parent_id() {
    // Bus set but parent_tool_id absent → no tee.  Mirrors the case
    // where a subagent ran from a code path without a parent dispatch
    // (tests, dream callbacks).  Without this guard the frontend would
    // get an event tagged with `parent_tool_id: ""` and either spam
    // top-level chips or attach to the wrong panel.
    let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::controller::http::SseEvent>(8);
    let bus = crate::controller::http::SubagentEventBus::new(tx);
    let mut cap = CaptureOutput::new().with_ui_sink(Some(bus), None);
    cap.tool_use_start("inner_1", "bash").unwrap();
    assert!(matches!(rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)),
        "no frame should have been sent");
}

#[test]
fn capture_output_text_path_does_not_tee_to_bus() {
    // The LLM-boundary invariant: text deltas (the child's reply text)
    // must NEVER reach the UI bus — they bubble up to the parent only
    // as the subagent tool's `ToolOutput.content`, where the parent's
    // own `SseOutput` decides what to render.  This test pins that
    // invariant so a future "stream subagent text live" PR has to
    // delete the assertion explicitly.
    let (mut cap, mut rx) = capture_with_bus("parent_subagent");
    cap.text_delta("hidden inner reasoning").unwrap();
    assert!(matches!(rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)));
    assert_eq!(cap.text(), "hidden inner reasoning");
}

#[test]
fn capture_output_inner_tool_id_resets_on_flush() {
    // `flush` runs at end-of-turn for `SseOutput`.  Mirror the same
    // contract here so a later turn doesn't accidentally tag results
    // with a stale tool_use_id from the previous turn.
    use crate::controller::http::SseEvent;
    let (mut cap, mut rx) = capture_with_bus("parent_subagent");
    cap.tool_use_start("inner_1", "bash").unwrap();
    let _ = rx.try_recv();
    cap.flush().unwrap();
    cap.tool_result(&ToolOutput::success("ok")).unwrap();
    match rx.try_recv().unwrap() {
        SseEvent::ToolResult { tool_use_id, .. } => {
            assert!(tool_use_id.is_none(), "stale inner id leaked through flush");
        }
        other => panic!("unexpected: {}", serde_json::to_string(&other).unwrap()),
    }
}

// -----------------------------------------------------------------------
// SubagentTool metadata tests
// -----------------------------------------------------------------------

#[test]
fn subagent_tool_name_and_description() {
    let config = SubagentAgentConfig {
        name: "research_agent".into(),
        description: "Research specialist".into(),
        system_prompt: "You are a researcher.".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    assert_eq!(tool.name(), "research_agent");
    assert_eq!(tool.description(), "Research specialist");
}

#[test]
fn subagent_tool_input_schema_has_required_task() {
    let config = SubagentAgentConfig {
        name: "test_agent".into(),
        description: "Test".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    let schema = tool.input_schema();
    assert_eq!(schema["properties"]["task"]["type"], "string");
    assert_eq!(schema["properties"]["context"]["type"], "string");
    assert_eq!(schema["required"][0], "task");
}

// -----------------------------------------------------------------------
// SubagentTool execution tests (with MockLlm)
// -----------------------------------------------------------------------

/// Mock LLM that returns pre-programmed responses for subagent tests.
struct MockLlm {
    responses: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
    /// Records the `model` field of every `CompletionConfig` the client
    /// receives so tests can assert which model a subagent billed.
    models_seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Records the `system` prompt the client is called with — used to
    /// assert which cheatsheets were injected by the orchestrator.
    systems_seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockLlm {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            models_seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            systems_seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Handle to the shared `models_seen` buffer; clone before wrapping
    /// the client in `Box<dyn LlmClient>` so the test can still read it.
    fn models_seen_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        std::sync::Arc::clone(&self.models_seen)
    }

    fn systems_seen_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        std::sync::Arc::clone(&self.systems_seen)
    }
}

#[async_trait]
impl crate::llm::LlmClient for MockLlm {
    async fn stream(
        &self,
        _messages: &[crate::message::Message],
        system: &str,
        _system_suffix: &str,
        _tools: &[crate::llm::ToolDefinition],
        config: &crate::llm::CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
        self.models_seen.lock().unwrap().push(config.model.clone());
        self.systems_seen.lock().unwrap().push(system.to_string());
        let events = self.responses.lock().unwrap().remove(0);
        Ok(crate::llm::StreamResponse {
            stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
            tool_mode: crate::llm::ToolMode::Execute,
            input_tokens: None,
        })
    }
}

/// Test the subagent flow by constructing a child Agent manually with
/// a MockLlm and running it — exercises the same code path as
/// SubagentTool::run().
#[tokio::test]
async fn subagent_runs_child_and_returns_result() {
    // Build a mock child agent that returns "Research complete."
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Research complete.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let settings = AgentSettings {
        api_key: "test".into(),
        system_prompt: "You are a researcher.".into(),
        ..Default::default()
    };

    let skills: Vec<Box<dyn Skill>> = vec![Box::new(FilteredSkill { tools: vec![] })];
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let mut agent =
        crate::agent::Agent::new(crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)), sandbox, skills, &settings, None, 0, None, None).unwrap();
    agent.set_depth(1);

    let mut capture = CaptureOutput::new();
    let result = agent
        .run("Research Rust patterns", &mut capture)
        .await
        .unwrap();

    assert_eq!(result, "Research complete.");
    assert_eq!(capture.text(), "Research complete.");
}

/// Regression: when the child LLM hits `max_tokens` mid-response, the
/// agent injects a continuation prompt and re-streams.  The returned
/// text (surfaced as `ToolOutput::success(final_text)` by `spawn_child`)
/// must include BOTH the truncated first chunk and the continuation —
/// otherwise long subagent reports are silently clipped to only the tail
/// turn.  See the pygoat run where a 27370-byte main report was followed
/// by a 1376-byte continuation and the return value was 1376 bytes.
#[tokio::test]
async fn subagent_concatenates_text_across_max_tokens_continuation() {
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::TextDelta("first-chunk ".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::MaxTokens,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("second-chunk".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let config = SubagentAgentConfig {
        name: "continuation_agent".into(),
        description: "Test".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: Some(5),
        max_tokens: Some(1024),
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({"task": "write a long answer"});
    let result = tool.run(&input, &ctx).await.unwrap();

    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert!(
        result.content.contains("first-chunk "),
        "return value lost the pre-continuation chunk: {:?}",
        result.content
    );
    assert!(
        result.content.contains("second-chunk"),
        "return value missing post-continuation chunk: {:?}",
        result.content
    );
    assert_eq!(result.content, "first-chunk second-chunk");
}

#[tokio::test]
async fn subagent_depth_limit_prevents_recursion() {
    let config = SubagentAgentConfig {
        name: "deep_agent".into(),
        description: "Too deep".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    // Create a context at max depth.
    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: MAX_SUBAGENT_DEPTH,
        dangerous_no_sandbox: false,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    tool_use_id: None,
    subagent_events: None,
    };

    let input = serde_json::json!({"task": "should fail"});
    let result = tool.run(&input, &ctx).await.unwrap();

    assert!(result.is_error);
    assert!(result.content.contains("Maximum subagent nesting depth"));
}

#[tokio::test]
async fn subagent_missing_task_returns_error() {
    let config = SubagentAgentConfig {
        name: "test_agent".into(),
        description: "Test".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({}); // No "task" field

    let result = tool.run(&input, &ctx).await;
    assert!(result.is_err());
}

// -----------------------------------------------------------------------
// FilteredSkill tests
// -----------------------------------------------------------------------

#[test]
fn filtered_skill_exposes_tools() {
    let tool: Arc<dyn Tool> = Arc::new(crate::tool::bash::BashTool::default());
    let skill = FilteredSkill { tools: vec![tool] };

    assert_eq!(skill.name(), "inherited");
    assert_eq!(skill.tools().len(), 1);
    assert_eq!(skill.tools()[0].name(), "bash");
}

// -----------------------------------------------------------------------
// SubagentSkill tests
// -----------------------------------------------------------------------

#[test]
fn subagent_skill_system_prompt_lists_agents() {
    // Create a minimal settings with a provider.
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "claude".to_string(),
        crate::config::ProviderConfig {
            provider_type: LlmProvider::Anthropic,
            models: vec!["claude-sonnet-4-20250514".into()],
            api_key: crate::auth::Credential::new(String::new()),
            base_url: None,
        },
    );

    let settings = crate::config::Settings {
        providers,
        ..Default::default()
    };

    let configs = vec![SubagentAgentConfig {
        name: "research_agent".into(),
        description: "Research specialist".into(),
        system_prompt: "You are a researcher.".into(),
        provider: "claude".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry, None);

    assert_eq!(skill.name(), "subagents");
    // 1 config-driven subagent + 1 coder + 1 security_engineer = 3
    assert_eq!(skill.tools().len(), 3);
    assert_eq!(skill.tools()[0].name(), "research_agent");
    assert_eq!(skill.tools()[1].name(), "coder");
    assert_eq!(skill.tools()[2].name(), "security_engineer");

    let prompt = skill.system_prompt().unwrap();
    assert!(prompt.contains("research_agent"));
    assert!(prompt.contains("Research specialist"));
    assert!(prompt.contains("subagents"));
    assert!(prompt.contains("coder"));
    assert!(prompt.contains("security_engineer"));
}

#[test]
fn subagent_skill_skips_unknown_provider() {
    let settings = crate::config::Settings::default(); // no providers

    let configs = vec![SubagentAgentConfig {
        name: "bad_agent".into(),
        description: "Unknown provider".into(),
        system_prompt: "Test".into(),
        provider: "nonexistent".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry, None);

    // Should have skipped the subagent with unknown provider,
    // but the built-in coder and security_engineer are always present.
    assert_eq!(skill.tools().len(), 2);
    assert_eq!(skill.tools()[0].name(), "coder");
    assert_eq!(skill.tools()[1].name(), "security_engineer");
    assert!(skill.system_prompt().unwrap().contains("coder"));
    assert!(skill.system_prompt().unwrap().contains("security_engineer"));
}

#[test]
fn name_allowlist_drops_coder_and_orchestrators_when_excluded() {
    // The SPA's tool-picker collapses builtins, coder, and the
    // orchestrator subagents (security_engineer) into one checklist.
    // When the operator's allowlist excludes those names, dyson must
    // skip registering them — otherwise the agent introspects them
    // as available even though the operator turned them off.
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "claude".to_string(),
        crate::config::ProviderConfig {
            provider_type: LlmProvider::Anthropic,
            models: vec!["claude-sonnet-4-20250514".into()],
            api_key: crate::auth::Credential::new(String::new()),
            base_url: None,
        },
    );
    let settings = crate::config::Settings {
        providers,
        ..Default::default()
    };

    let configs = vec![SubagentAgentConfig {
        name: "research_agent".into(),
        description: "Research specialist".into(),
        system_prompt: "You are a researcher.".into(),
        provider: "claude".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);

    // Allowlist contains only the user-defined subagent and a couple
    // of (irrelevant) builtin tool names.  coder + security_engineer
    // are NOT in the list; they must be dropped.
    let allow: std::collections::HashSet<String> = [
        "research_agent".to_string(),
        "read_file".to_string(),
        "write_file".to_string(),
    ]
    .into_iter()
    .collect();
    let skill = SubagentSkill::new(
        &configs,
        &settings,
        sandbox,
        None,
        &[],
        &registry,
        Some(&allow),
    );

    // Only research_agent survives — coder and security_engineer are
    // gone because they weren't in the allowlist.
    let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["research_agent"]);
    let prompt = skill.system_prompt().unwrap();
    assert!(prompt.contains("research_agent"));
    assert!(!prompt.contains("- **coder**"),
        "coder must not appear in the prompt when filtered out");
    assert!(!prompt.contains("- **security_engineer**"),
        "security_engineer must not appear in the prompt when filtered out");
}

#[test]
fn name_allowlist_keeps_only_listed_orchestrators() {
    // Allowlist includes coder but not security_engineer — coder
    // survives, the orchestrator gets dropped.
    let settings = crate::config::Settings::default();
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let allow: std::collections::HashSet<String> =
        ["coder".to_string()].into_iter().collect();
    let skill = SubagentSkill::new(
        &[],
        &settings,
        sandbox,
        None,
        &[],
        &registry,
        Some(&allow),
    );
    let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["coder"]);
}

#[test]
fn name_allowlist_none_preserves_default_registration() {
    // Sanity: passing None (no allowlist) keeps the pre-existing
    // behaviour — coder + every orchestrator register unconditionally.
    let settings = crate::config::Settings::default();
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&[], &settings, sandbox, None, &[], &registry, None);
    let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
    assert!(names.contains(&"coder"),
        "coder must be present when no allowlist is supplied");
    assert!(names.contains(&"security_engineer"),
        "security_engineer must be present when no allowlist is supplied");
}

// -----------------------------------------------------------------------
// Tool filtering tests
// -----------------------------------------------------------------------

#[test]
fn filter_tools_none_inherits_all() {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
    ];
    let filtered = filter_tools(&tools, &None);
    assert_eq!(filtered.len(), 2);
}

#[test]
fn filter_tools_by_name() {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
        Arc::new(crate::tool::write_file::WriteFileTool),
    ];
    let filtered = filter_tools(&tools, &Some(vec!["bash".into(), "read_file".into()]));
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].name(), "bash");
    assert_eq!(filtered[1].name(), "read_file");
}

#[test]
fn filter_tools_ignores_unknown_names() {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tool::bash::BashTool::default())];
    let filtered = filter_tools(&tools, &Some(vec!["bash".into(), "nonexistent".into()]));
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].name(), "bash");
}

#[test]
fn filter_tools_empty_filter_returns_none() {
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(crate::tool::bash::BashTool::default())];
    let filtered = filter_tools(&tools, &Some(vec![]));
    assert_eq!(filtered.len(), 0);
}

// -----------------------------------------------------------------------
// Built-in subagent tests
// -----------------------------------------------------------------------

#[test]
fn builtin_subagent_configs_returns_expected_set() {
    let configs = builtin_subagent_configs();
    let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["planner", "researcher", "verifier", "dependency_review"]
    );
    // All use the "default" provider.
    assert!(configs.iter().all(|c| c.provider == "default"));
}

#[test]
fn builtin_subagents_have_tool_filters() {
    let configs = builtin_subagent_configs();
    // Planner has read-only tools.
    let planner_tools = configs[0].tools.as_ref().unwrap();
    assert!(planner_tools.contains(&"read_file".to_string()));
    assert!(!planner_tools.contains(&"bash".to_string()));
    // Researcher has broader access.
    let researcher_tools = configs[1].tools.as_ref().unwrap();
    assert!(researcher_tools.contains(&"bash".to_string()));
    assert!(researcher_tools.contains(&"web_search".to_string()));
    // Verifier has bash + read-only tools but no web_search.
    let verifier_tools = configs[2].tools.as_ref().unwrap();
    assert!(verifier_tools.contains(&"bash".to_string()));
    assert!(verifier_tools.contains(&"read_file".to_string()));
    assert!(!verifier_tools.contains(&"web_search".to_string()));
}

#[test]
fn verifier_subagent_has_higher_limits() {
    let configs = builtin_subagent_configs();
    let verifier = &configs[2];
    assert_eq!(verifier.name, "verifier");
    // Verifier needs more iterations and tokens for thorough checking.
    assert_eq!(verifier.max_iterations, Some(25));
    assert_eq!(verifier.max_tokens, Some(8192));
}

#[test]
fn verifier_system_prompt_requires_verdict() {
    let configs = builtin_subagent_configs();
    let verifier = &configs[2];
    assert!(verifier.system_prompt.contains("VERDICT: PASS"));
    assert!(verifier.system_prompt.contains("VERDICT: FAIL"));
    assert!(verifier.system_prompt.contains("VERDICT: PARTIAL"));
}

#[test]
fn injects_protocol_fragment_appended_to_system_prompt() {
    // Protocol injection is now data-driven — any subagent whose config
    // sets `injects_protocol: Some(...)` contributes its fragment to the
    // parent's subagent system prompt.  The subagent's *name* no longer
    // matters (used to be hard-coded to "verifier").
    let settings = crate::config::Settings {
        agent: AgentSettings {
            provider: LlmProvider::Anthropic,
            api_key: crate::auth::Credential::new("test-key".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    let configs = vec![SubagentAgentConfig {
        name: "checker".into(), // deliberately not "verifier"
        description: "Test checker".into(),
        system_prompt: "You check.".into(),
        provider: "default".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: Some("\n\n## Usage Protocol\nAlways invoke me first.".into()),
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry, None);

    let prompt = skill.system_prompt().unwrap();
    assert!(prompt.contains("Usage Protocol"));
    assert!(prompt.contains("Always invoke me first."));
}

#[test]
fn verification_protocol_absent_without_verifier() {
    let settings = crate::config::Settings {
        agent: AgentSettings {
            provider: LlmProvider::Anthropic,
            api_key: crate::auth::Credential::new("test-key".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    let configs = vec![SubagentAgentConfig {
        name: "custom_agent".into(),
        description: "Not a verifier".into(),
        system_prompt: "You do things.".into(),
        provider: "default".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry, None);

    let prompt = skill.system_prompt().unwrap();
    assert!(!prompt.contains("Verification Protocol"));
}

#[test]
fn default_provider_resolves_to_agent_settings() {
    // Create settings with no named providers but a configured agent.
    let settings = crate::config::Settings {
        agent: AgentSettings {
            provider: LlmProvider::Anthropic,
            api_key: crate::auth::Credential::new("test-key".into()),
            base_url: Some("https://custom.api".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    let configs = vec![SubagentAgentConfig {
        name: "test_default".into(),
        description: "Uses default provider".into(),
        system_prompt: "Test".into(),
        provider: "default".into(),
        model: None,
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry, None);

    // Should have resolved successfully (1 config-driven + 1 coder + 1 security_engineer = 3 tools).
    assert_eq!(skill.tools().len(), 3);
    assert_eq!(skill.tools()[0].name(), "test_default");
    assert_eq!(skill.tools()[1].name(), "coder");
    assert_eq!(skill.tools()[2].name(), "security_engineer");
}

/// Regression: `SubagentTool` must bill the parent's model when its own
/// `config.model` is unset.  Before this fix it silently fell back to
/// the provider registry's hardcoded Sonnet, which billed users for a
/// model they never configured.
#[tokio::test]
async fn subagent_uses_parent_model_when_config_model_unset() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let seen = llm.models_seen_handle();

    let config = SubagentAgentConfig {
        name: "test_sub".into(),
        description: "Test".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None, // unset → should inherit parent_model
        max_iterations: None,
        max_tokens: None,
        tools: None,
        injects_protocol: None,
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(), // parent's model
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({"task": "do something"});
    let _ = tool.run(&input, &ctx).await.unwrap();

    let models = seen.lock().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], "claude-opus-4-20250514");
}

/// Regression: `CoderTool` must bill the parent's model, not a registry default.
#[tokio::test]
async fn coder_uses_parent_model() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let seen = llm.models_seen_handle();

    let tmp = tempfile::tempdir().unwrap();
    let tool = CoderTool::new(
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
    );

    let ctx = ToolContext::for_test(tmp.path());
    let input = serde_json::json!({"path": ".", "task": "refactor"});
    let _ = tool.run(&input, &ctx).await.unwrap();

    let models = seen.lock().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], "claude-opus-4-20250514");
}

/// Regression: `OrchestratorTool` must bill the parent's model, not a registry default.
#[tokio::test]
async fn orchestrator_uses_parent_model() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let seen = llm.models_seen_handle();

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({"task": "audit auth"});
    let _ = tool.run(&input, &ctx).await.unwrap();

    let models = seen.lock().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], "claude-opus-4-20250514");
}

// -----------------------------------------------------------------------
// CoderTool tests
// -----------------------------------------------------------------------

/// Helper to create a CoderTool with no inherited tools for metadata tests.
fn make_coder_tool() -> CoderTool {
    CoderTool::new(
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
    )
}

#[test]
fn coder_tool_name_and_description() {
    let tool = make_coder_tool();
    assert_eq!(tool.name(), "coder");
    assert!(!tool.description().is_empty());
    assert!(tool.description().contains("directory"));
}

#[test]
fn coder_tool_input_schema_has_required_fields() {
    let tool = make_coder_tool();
    let schema = tool.input_schema();
    assert_eq!(schema["properties"]["path"]["type"], "string");
    assert_eq!(schema["properties"]["task"]["type"], "string");
    assert_eq!(schema["required"][0], "path");
    assert_eq!(schema["required"][1], "task");
    // Should NOT have a "context" property (that's SubagentTool's schema).
    assert!(schema["properties"]["context"].is_null());
}

#[tokio::test]
async fn coder_depth_limit_prevents_recursion() {
    let tool = make_coder_tool();

    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: MAX_SUBAGENT_DEPTH,
        dangerous_no_sandbox: false,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    tool_use_id: None,
    subagent_events: None,
    };

    let input = serde_json::json!({"path": ".", "task": "should fail"});
    let result = tool.run(&input, &ctx).await.unwrap();

    assert!(result.is_error);
    assert!(result.content.contains("Maximum subagent nesting depth"));
}

#[tokio::test]
async fn coder_missing_path_returns_error() {
    let tool = make_coder_tool();
    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({"task": "fix bug"});

    let result = tool.run(&input, &ctx).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn coder_missing_task_returns_error() {
    let tool = make_coder_tool();
    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({"path": "."});

    let result = tool.run(&input, &ctx).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn coder_nonexistent_path_returns_error() {
    let tool = make_coder_tool();
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ToolContext::for_test(tmp.path());
    let input = serde_json::json!({"path": "no_such_dir", "task": "fix"});

    // resolve_and_validate_path succeeds (designed for write ops), but the
    // is_dir() check catches the non-existent path.
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("not a directory"));
}

#[tokio::test]
async fn coder_path_not_directory_returns_error() {
    let tool = make_coder_tool();
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("afile.txt");
    std::fs::write(&file_path, "content").unwrap();

    let ctx = ToolContext::for_test(tmp.path());
    let input = serde_json::json!({"path": "afile.txt", "task": "fix"});

    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("not a directory"));
}

#[test]
fn coder_filters_to_correct_tools() {
    // Provide a superset of tools — coder should filter to only its allowed set.
    let parent_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
        Arc::new(crate::tool::write_file::WriteFileTool),
        Arc::new(crate::tool::edit_file::EditFileTool),
        Arc::new(crate::tool::list_files::ListFilesTool),
        Arc::new(crate::tool::search_files::SearchFilesTool),
        Arc::new(crate::tool::send_file::SendFileTool),
        Arc::new(crate::tool::bulk_edit::BulkEditTool),
    ];

    let tool = CoderTool::new(
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &parent_tools,
    );

    let names: Vec<&str> = tool.inherited_tools.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 6);
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"edit_file"));
    assert!(names.contains(&"list_files"));
    assert!(names.contains(&"search_files"));
    assert!(names.contains(&"bulk_edit"));
    // write_file and send_file should be excluded.
    assert!(!names.contains(&"write_file"));
    assert!(!names.contains(&"send_file"));
}

#[tokio::test]
async fn coder_runs_child_and_returns_result() {
    // Build a mock LLM that returns "Changes complete." without calling tools.
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Changes complete.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let tmp = tempfile::tempdir().unwrap();
    let sub_dir = tmp.path().join("src");
    std::fs::create_dir(&sub_dir).unwrap();

    let tool = CoderTool::new(
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
    );

    let ctx = ToolContext::for_test(tmp.path());
    let input = serde_json::json!({
        "path": "src",
        "task": "Rename Config to AuthConfig"
    });

    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "Changes complete.");
}

// -----------------------------------------------------------------------
// OrchestratorTool tests
// -----------------------------------------------------------------------

#[test]
fn orchestrator_tool_uses_config_name_and_description() {
    let config = OrchestratorConfig {
        name: "test_orchestrator",
        description: "A test orchestrator",
        system_prompt: "You are a test.",
        direct_tool_names: &["bash"],
        max_iterations: 10,
        max_tokens: 4096,
        injects_protocol: None,
        inject_cheatsheets: false,
        emit_artefact: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );
    assert_eq!(tool.name(), "test_orchestrator");
    assert_eq!(tool.description(), "A test orchestrator");
}

#[test]
fn security_engineer_config_produces_correct_values() {
    let config = security_engineer_config();
    assert_eq!(config.name, "security_engineer");
    assert!(config.description.contains("security"));
    assert!(config.system_prompt.contains("ast_query"));
    assert_eq!(config.max_iterations, 80);
    assert_eq!(config.max_tokens, 8192);
    assert!(config.injects_protocol.is_some());
    assert!(config.direct_tool_names.contains(&"ast_query"));
    assert!(config.direct_tool_names.contains(&"attack_surface_analyzer"));
    assert!(config.direct_tool_names.contains(&"exploit_builder"));
}

#[test]
fn orchestrator_filters_to_config_tool_names() {
    let parent_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
        Arc::new(crate::tool::write_file::WriteFileTool),
        Arc::new(crate::tool::edit_file::EditFileTool),
        Arc::new(crate::tool::list_files::ListFilesTool),
        Arc::new(crate::tool::search_files::SearchFilesTool),
        Arc::new(crate::tool::send_file::SendFileTool),
        Arc::new(crate::tool::security::AstQueryTool),
        Arc::new(crate::tool::security::AttackSurfaceAnalyzerTool),
        Arc::new(crate::tool::security::ExploitBuilderTool),
    ];

    let inner_subagents: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()), // stand-in
    ];

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &parent_tools,
        inner_subagents,
    );

    let direct_names: Vec<&str> = tool.direct_tools.iter().map(|t| t.name()).collect();
    assert_eq!(direct_names.len(), 7);
    assert!(direct_names.contains(&"bash"));
    assert!(direct_names.contains(&"read_file"));
    assert!(direct_names.contains(&"search_files"));
    assert!(direct_names.contains(&"list_files"));
    assert!(direct_names.contains(&"ast_query"));
    assert!(direct_names.contains(&"attack_surface_analyzer"));
    assert!(direct_names.contains(&"exploit_builder"));
    // write_file, edit_file, send_file should be excluded.
    assert!(!direct_names.contains(&"write_file"));
    assert!(!direct_names.contains(&"edit_file"));
    assert!(!direct_names.contains(&"send_file"));

    // Inner subagent tools are passed through as-is.
    assert_eq!(tool.inner_subagent_tools.len(), 1);
}

#[tokio::test]
async fn orchestrator_depth_limit_prevents_recursion() {
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext {
        working_dir: std::env::current_dir().unwrap(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: MAX_SUBAGENT_DEPTH,
        dangerous_no_sandbox: false,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    tool_use_id: None,
    subagent_events: None,
    };

    let input = serde_json::json!({"task": "should fail"});
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("Maximum subagent nesting depth"));
}

#[tokio::test]
async fn orchestrator_runs_child_and_returns_result() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Security review complete. No critical issues found.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "Review auth module",
        "context": "Recently added OAuth2"
    });

    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "Security review complete. No critical issues found.");
}

// Programmatic artefact emission regression test.  Prior to this change,
// `looks_like_report` (>500 chars + leading `#`) silently suppressed
// artefact emission when a weaker model produced a short or
// unformatted reply, so the UI's Artefacts tab went empty even on a
// successful review.  The contract is now: if `emit_artefact` is set
// and the run didn't error and content is non-empty, emit — regardless
// of shape.
#[tokio::test]
async fn orchestrator_emits_artefact_for_non_report_shaped_output() {
    // Short, non-markdown reply — would have failed the old heuristic
    // (48 chars, no leading `#`).  Still must produce an artefact.
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("Security review complete. No critical issues found.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({ "task": "Review the module" });
    let result = tool.run(&input, &ctx).await.unwrap();

    assert!(!result.is_error);
    assert_eq!(
        result.artefacts.len(),
        1,
        "expected 1 artefact from short non-report reply, got {}",
        result.artefacts.len(),
    );
    let art = &result.artefacts[0];
    assert!(matches!(art.kind, crate::message::ArtefactKind::SecurityReview));
    assert_eq!(art.content, "Security review complete. No critical issues found.");
    assert!(art.title.starts_with("Security review: "));
}

// Whitespace-only content must NOT produce an artefact — the single
// remaining gate (empty after `trim()`) prevents stashing blank
// reports in the Artefacts tab when a model returns a pad-only reply.
#[tokio::test]
async fn orchestrator_suppresses_artefact_when_output_is_whitespace_only() {
    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("   \n  \t  \n".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({ "task": "Review the module" });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(result.artefacts.is_empty(), "whitespace-only content should not emit");
}

#[test]
fn orchestrator_with_custom_config() {
    // Demonstrate composability: any role can be an orchestrator.
    let config = OrchestratorConfig {
        name: "devops_engineer",
        description: "Infrastructure and deployment specialist",
        system_prompt: "You are a devops engineer.",
        direct_tool_names: &["bash", "read_file"],
        max_iterations: 20,
        max_tokens: 4096,
        injects_protocol: Some("\n## DevOps Protocol\nUse for infra changes."),
        inject_cheatsheets: false,
        emit_artefact: None,
    };

    let parent_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
        Arc::new(crate::tool::write_file::WriteFileTool),
    ];

    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &parent_tools,
        vec![],
    );

    assert_eq!(tool.name(), "devops_engineer");
    assert_eq!(tool.direct_tools.len(), 2);
    let names: Vec<&str> = tool.direct_tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read_file"));
    assert!(!names.contains(&"write_file"));
    assert!(tool.config().injects_protocol.is_some());
}

#[test]
fn builtin_orchestrator_configs_includes_security_engineer() {
    let configs = builtin_orchestrator_configs();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].name, "security_engineer");
}

// -----------------------------------------------------------------------
// OrchestratorTool `path` input tests
//
// The `path` field scopes the child agent's working directory.  This
// matters any time the orchestrator is invoked with a target location
// that differs from the parent's working directory — most obviously the
// `examples/expensive_live_security_review.rs` harness, but also any
// future controller that routes a "review /abs/dir" message through the
// security_engineer tool.
// -----------------------------------------------------------------------

#[tokio::test]
async fn orchestrator_rejects_nonexistent_path() {
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );
    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "any",
        "path": "/this/path/definitely/does/not/exist/dyson-test"
    });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(result.is_error, "expected error for nonexistent path");
    assert!(
        result.content.contains("cannot be resolved"),
        "unexpected error text: {}",
        result.content
    );
}

#[tokio::test]
async fn orchestrator_rejects_path_pointing_to_file() {
    // Write a temp file, then try to scope the orchestrator at it.
    let file = std::env::temp_dir().join(format!(
        "dyson-orch-test-{}.tmp",
        std::process::id()
    ));
    std::fs::write(&file, b"not a directory").unwrap();

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(
            crate::llm::create_client(&crate::config::AgentSettings::default(), None, false),
        ),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );
    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "any",
        "path": file.display().to_string(),
    });
    let result = tool.run(&input, &ctx).await.unwrap();
    let _ = std::fs::remove_file(&file);
    assert!(result.is_error, "expected error for file path");
    assert!(
        result.content.contains("not a directory"),
        "unexpected error text: {}",
        result.content
    );
}

/// Spy tool that records `ctx.working_dir` when invoked, so a test can
/// assert the orchestrator forwarded its `path` input into the child
/// agent's working directory.
struct WorkingDirSpy {
    captured: std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

#[async_trait]
impl Tool for WorkingDirSpy {
    fn name(&self) -> &str {
        "working_dir_spy"
    }
    fn description(&self) -> &str {
        "Records ctx.working_dir (test-only)."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn run(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        *self.captured.lock().unwrap() = Some(ctx.working_dir.clone());
        Ok(ToolOutput::success("captured"))
    }
}

#[tokio::test]
async fn orchestrator_propagates_path_to_child_working_dir() {
    // Use $TMPDIR directly as the scoped path — guaranteed to exist
    // and distinct from the process cwd.
    let target = std::env::temp_dir().canonicalize().unwrap();

    // MockLlm: one tool call to `working_dir_spy`, then end.
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
                input: serde_json::json!({}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let spy: Arc<dyn Tool> = Arc::new(WorkingDirSpy {
        captured: std::sync::Arc::clone(&captured),
    });

    // Minimal config: allow only the spy as a direct tool so the child
    // has exactly one tool available.
    let config = OrchestratorConfig {
        name: "scope_test",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &["working_dir_spy"],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        inject_cheatsheets: false,
        emit_artefact: None,
    };

    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        std::slice::from_ref(&spy),
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "call the spy",
        "path": target.display().to_string(),
    });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error, "unexpected error: {}", result.content);

    let captured_dir = captured
        .lock()
        .unwrap()
        .clone()
        .expect("spy was never invoked");
    assert_eq!(
        captured_dir, target,
        "child's working_dir should equal the scoped path"
    );
}

/// Regression: `SubagentTool::run` must forward the calling context's
/// `working_dir` into its child agent.  Without this, inner subagents
/// dispatched from inside a `security_engineer` orchestrator (which
/// scopes its own child to the target repo via `--path`) fall back to
/// the process cwd instead of the target scope.  Symptom in the wild:
/// `dependency_review` spawned from within a `security_engineer` review
/// of `nextjs-14.0.0` reported findings from `juice-shop` instead —
/// the only lockfile `dependency_scan` could reach was the Juice Shop
/// one left in the process cwd by a previous smoke run, because the
/// subagent lost scope on the jump from orchestrator-child to inner
/// subagent.  See docs/sample-seceng-reports/iter1-nextjs-14.0.0-hit.md
/// and iter2-react-server-dom-webpack-still-miss.md for the reports
/// that exposed this.
#[tokio::test]
async fn subagent_inherits_parents_working_dir() {
    // Scoped path distinct from the process cwd — temp dir is the
    // standard choice here (orchestrator test above uses the same).
    let scoped = std::env::temp_dir().canonicalize().unwrap();
    assert_ne!(
        scoped,
        std::env::current_dir().unwrap(),
        "test precondition: temp_dir must differ from cwd",
    );

    // MockLlm: the child makes one call to the spy, then ends.
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
                input: serde_json::json!({}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let spy: Arc<dyn Tool> = Arc::new(WorkingDirSpy {
        captured: std::sync::Arc::clone(&captured),
    });

    let config = SubagentAgentConfig {
        name: "scoped_subagent".into(),
        description: "Test".into(),
        system_prompt: "Test".into(),
        provider: "anthropic".into(),
        model: None,
        max_iterations: Some(3),
        max_tokens: Some(1024),
        tools: None,
        injects_protocol: None,
    };
    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        vec![spy],
    );

    // Context mirrors what an OrchestratorTool child would pass when it
    // dispatched an inner subagent: its own scoped working_dir.
    let ctx = ToolContext {
        working_dir: scoped.clone(),
        env: std::collections::HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: 1,
        dangerous_no_sandbox: false,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
    tool_use_id: None,
    subagent_events: None,
    };

    let input = serde_json::json!({ "task": "call the spy" });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error, "unexpected error: {}", result.content);

    let captured_dir = captured
        .lock()
        .unwrap()
        .clone()
        .expect("spy was never invoked");
    assert_eq!(
        captured_dir, scoped,
        "inner subagent's working_dir should inherit the caller's ctx.working_dir \
         (got {}, expected {})",
        captured_dir.display(),
        scoped.display(),
    );
}

#[tokio::test]
async fn orchestrator_without_path_keeps_process_cwd() {
    // No `path` in input → child inherits the process cwd, matching
    // the pre-existing behavior that controllers already depend on.
    let llm = MockLlm::new(vec![
        vec![
            StreamEvent::ToolUseStart {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "call_1".into(),
                name: "working_dir_spy".into(),
                input: serde_json::json!({}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: None,
            },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                output_tokens: None,
            },
        ],
    ]);

    let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
    let spy: Arc<dyn Tool> = Arc::new(WorkingDirSpy {
        captured: std::sync::Arc::clone(&captured),
    });

    let config = OrchestratorConfig {
        name: "scope_test",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &["working_dir_spy"],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        inject_cheatsheets: false,
        emit_artefact: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        std::slice::from_ref(&spy),
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({ "task": "call the spy" });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error);

    let captured_dir = captured
        .lock()
        .unwrap()
        .clone()
        .expect("spy was never invoked");
    // Child defaults to process cwd (std::env::current_dir) when no
    // `path` is passed.  The orchestrator's own ctx.working_dir is NOT
    // used in that fallback path, matching the previous behaviour.
    let process_cwd = std::env::current_dir().unwrap();
    assert_eq!(captured_dir, process_cwd);
}

// -----------------------------------------------------------------------
// Cheatsheet injection — integration between repo_detect and the
// security_engineer OrchestratorTool.  These tests run the real
// orchestrator `run()` with a MockLlm that records the system prompt,
// so they cover the full composition path: detection → compose →
// concatenate onto the base security_engineer.md prompt.
// -----------------------------------------------------------------------

#[tokio::test]
async fn security_engineer_injects_express_cheatsheet_for_js_repo() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"name":"demo","dependencies":{"express":"^4.18"}}"#,
    )
    .unwrap();
    let target = tmp.path().canonicalize().unwrap();

    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("# Security Review: demo\n\nNo findings.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let systems = llm.systems_seen_handle();

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "Audit",
        "path": target.display().to_string(),
    });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(!result.is_error, "orch error: {}", result.content);

    let seen = systems.lock().unwrap();
    assert_eq!(seen.len(), 1, "expected one child LLM turn");
    let system = &seen[0];
    // The base security_engineer.md content is still present.
    assert!(
        system.contains("Response shape"),
        "base security_engineer prompt missing"
    );
    // The JS lang sheet and Express framework sheet were appended.
    assert!(
        system.contains("Cheatsheet: lang/javascript"),
        "lang/javascript sheet missing from composed prompt"
    );
    assert!(
        system.contains("Cheatsheet: framework/express"),
        "framework/express sheet missing from composed prompt"
    );
}

#[tokio::test]
async fn security_engineer_injects_nothing_for_repo_without_manifests() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("README.md"), "# demo").unwrap();
    let target = tmp.path().canonicalize().unwrap();

    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("# done".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let systems = llm.systems_seen_handle();

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "Audit",
        "path": target.display().to_string(),
    });
    let _ = tool.run(&input, &ctx).await.unwrap();

    let seen = systems.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let system = &seen[0];
    // Base prompt present; no cheatsheet section added when no langs
    // were detected — that's the "no manifests" invariant.
    assert!(system.contains("Response shape"));
    assert!(
        !system.contains("Language and framework cheatsheets"),
        "unexpected cheatsheet header on manifest-free repo"
    );
}

#[tokio::test]
async fn orchestrator_without_inject_cheatsheets_flag_skips_detection() {
    // Custom orchestrator with inject_cheatsheets: false — even when
    // pointed at a JS repo, its system prompt must stay exactly the
    // configured literal.  Confirms scope is security_engineer-only.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"dependencies":{"express":"^4"}}"#,
    )
    .unwrap();
    let target = tmp.path().canonicalize().unwrap();

    let llm = MockLlm::new(vec![vec![
        StreamEvent::TextDelta("ok".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]]);
    let systems = llm.systems_seen_handle();

    let config = OrchestratorConfig {
        name: "no_sheets_orch",
        description: "test",
        system_prompt: "BASE_PROMPT_SENTINEL",
        direct_tool_names: &[],
        max_iterations: 2,
        max_tokens: 512,
        injects_protocol: None,
        inject_cheatsheets: false,
        emit_artefact: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({
        "task": "x",
        "path": target.display().to_string(),
    });
    let _ = tool.run(&input, &ctx).await.unwrap();

    let seen = systems.lock().unwrap();
    assert_eq!(seen.len(), 1);
    // The agent loop appends a short model/provider suffix — accept
    // that, but assert nothing from the cheatsheet composer leaked in.
    assert!(
        seen[0].starts_with("BASE_PROMPT_SENTINEL"),
        "system prompt did not start with orchestrator's configured base: {}",
        seen[0]
    );
    assert!(
        !seen[0].contains("Language and framework cheatsheets"),
        "cheatsheet header appeared even though inject_cheatsheets was false"
    );
}
