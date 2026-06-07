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
fn capture_with_bus(
    parent_tool_id: &str,
) -> (
    CaptureOutput,
    tokio::sync::broadcast::Receiver<crate::controller::http::SseEvent>,
) {
    let (tx, rx) = tokio::sync::broadcast::channel(64);
    let bus = crate::controller::http::SubagentEventBus::new(tx);
    let cap = CaptureOutput::new().with_ui_sink(Some(bus), Some(parent_tool_id.to_string()));
    (cap, rx)
}

#[test]
fn capture_output_tees_inner_tool_start_with_parent_id() {
    use crate::controller::http::SseEvent;
    let (mut cap, mut rx) = capture_with_bus("parent_subagent");
    cap.tool_use_start("inner_1", "bash").unwrap();
    match rx.try_recv().unwrap() {
        SseEvent::ToolStart {
            id,
            name,
            parent_tool_id,
        } => {
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
        SseEvent::ToolResult {
            content,
            is_error,
            parent_tool_id,
            tool_use_id,
            ..
        } => {
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
    assert!(
        matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "no frame should have been sent"
    );
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
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    /// Fallback text returned once `responses` is empty. Tests that fan
    /// out across many subagents (e.g. the unconditional per-class hunt
    /// waves) set this to a generic empty-findings response so the test
    /// doesn't have to enumerate one mock per taxonomy class.  Stored as
    /// a string (not a pre-built event vec) because `StreamEvent` is
    /// move-only.
    fallback: std::sync::Mutex<Option<String>>,
    /// Records the `model` field of every `CompletionConfig` the client
    /// receives so tests can assert which model a subagent billed.
    models_seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Records the `system` prompt the client is called with — used to
    /// assert the composed system prompt a subagent was given.
    systems_seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockLlm {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            fallback: std::sync::Mutex::new(None),
            models_seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            systems_seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Set the fallback text returned after the explicit `responses`
    /// list is exhausted. Used by tests where the harness fans out one
    /// specialist per taxonomy class and we don't care what the extra
    /// specialists reply — they just need to return something parseable.
    fn with_fallback(self, fallback_text: impl Into<String>) -> Self {
        *self.fallback.lock().unwrap() = Some(fallback_text.into());
        self
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

fn mock_text_response(text: impl Into<String>) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: None,
        },
    ]
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
        let events = {
            let mut queue = self.responses.lock().unwrap();
            if queue.is_empty() {
                let fallback_text = self
                    .fallback
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("MockLlm response queue exhausted and no fallback set");
                mock_text_response(fallback_text)
            } else {
                queue.remove(0)
            }
        };
        Ok(crate::llm::StreamResponse {
            stream: Box::pin(tokio_stream::iter(events.into_iter().map(Ok))),
            tool_mode: crate::llm::ToolMode::Execute,
            input_tokens: None,
            swarm_llm_audit_id: None,
            provider: None,
            model: None,
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
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
    let mut agent = crate::agent::Agent::new(
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        sandbox,
        skills,
        &settings,
        None,
        0,
        None,
        None,
    )
    .unwrap();
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
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        sandbox_bypass: None,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
        tool_use_id: None,
        subagent_events: None,
        artefacts: None,
        current_chat_id: None,
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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
    assert!(
        !prompt.contains("- **coder**"),
        "coder must not appear in the prompt when filtered out"
    );
    assert!(
        !prompt.contains("- **security_engineer**"),
        "security_engineer must not appear in the prompt when filtered out"
    );
}

#[test]
fn name_allowlist_keeps_only_listed_orchestrators() {
    // Allowlist includes coder but not security_engineer — coder
    // survives, the orchestrator gets dropped.
    let settings = crate::config::Settings::default();
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let allow: std::collections::HashSet<String> = ["coder".to_string()].into_iter().collect();
    let skill = SubagentSkill::new(&[], &settings, sandbox, None, &[], &registry, Some(&allow));
    let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["coder"]);
}

#[test]
fn name_allowlist_none_preserves_default_registration() {
    // Sanity: passing None (no allowlist) keeps the pre-existing
    // behaviour — coder + every orchestrator register unconditionally.
    let settings = crate::config::Settings::default();
    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
    let registry = crate::controller::ClientRegistry::new(&settings, None);
    let skill = SubagentSkill::new(&[], &settings, sandbox, None, &[], &registry, None);
    let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&"coder"),
        "coder must be present when no allowlist is supplied"
    );
    assert!(
        names.contains(&"security_engineer"),
        "security_engineer must be present when no allowlist is supplied"
    );
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
        crate::sandbox::SandboxBypassGuard::for_test(),
    ));
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
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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

    let config = OrchestratorConfig {
        name: "test_orchestrator",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &[],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        emit_artefact: None,
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        sandbox_bypass: None,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
        tool_use_id: None,
        subagent_events: None,
        artefacts: None,
        current_chat_id: None,
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        emit_artefact: None,
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    assert!(config.description.contains("staged"));
    assert!(config.system_prompt.contains("ast_query"));
    assert_eq!(config.max_iterations, 80);
    assert_eq!(config.max_tokens, 8192);
    assert!(config.injects_protocol.is_some());
    assert_eq!(
        config.harness,
        Some(orchestrator::OrchestratorHarness::SecurityResearch)
    );
    assert!(config.direct_tool_names.contains(&"ast_query"));
    assert!(
        config
            .direct_tool_names
            .contains(&"attack_surface_analyzer")
    );
    assert!(config.direct_tool_names.contains(&"exploit_builder"));
}

#[test]
fn security_engineer_resume_schema_does_not_require_task() {
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        None,
        &[],
        vec![],
    );
    let schema = tool.input_schema();
    assert_eq!(schema["properties"]["task"]["type"], "string");
    assert!(schema["required"].as_array().unwrap().is_empty());
}

#[test]
fn security_engineer_config_describes_staged_harness() {
    let stages: Vec<&str> = security_engineer::harness_stages()
        .iter()
        .map(|stage| stage.as_str())
        .collect();
    assert_eq!(
        stages,
        vec![
            "recon", "hunt", "validate", "gapfill", "dedupe", "trace", "feedback", "report"
        ]
    );
}

#[test]
fn security_engineer_taxonomy_includes_expanded_vulnerability_classes() {
    let class_ids: Vec<&str> = security_engineer::vulnerability_taxonomy()
        .iter()
        .map(|class| class.id)
        .collect();
    for expected in [
        "auth_authorization",
        "session_oauth_csrf",
        "ssrf_outbound_network",
        "proxy_http_boundary",
        "container_sandbox_runtime",
        "secrets_credentials",
        "persistence_lifecycle",
        "webhooks_inbound_integrations",
        "file_archive_path",
        "injection_unsafe_execution",
        "dependency_supply_chain",
        "crypto_randomness",
        "multi_tenant_isolation",
        "resource_exhaustion_dos",
        "frontend_security_ux",
        "agent_tool_boundary",
        "api_contract_input_validation",
        "audit_observability_forensics",
        "ci_cd_release_integrity",
        "data_retention_privacy",
        "race_condition_toctou",
        "business_logic_abuse",
        "mass_assignment_overposting",
        "denial_of_wallet_cost_abuse",
    ] {
        assert!(class_ids.contains(&expected), "missing {expected}");
    }
}

#[test]
fn security_engineer_prompts_include_all_harness_stages() {
    let config = security_engineer_config();
    let protocol = config.injects_protocol.unwrap();
    for stage in [
        "Recon", "Hunt", "Validate", "Gapfill", "Dedupe", "Trace", "Feedback", "Report",
    ] {
        assert!(
            config.system_prompt.contains(stage),
            "system prompt missing {stage}"
        );
        assert!(protocol.contains(stage), "protocol prompt missing {stage}");
    }
    assert!(protocol.contains("resume"));
    assert!(protocol.contains("checkpoint"));
    for phrase in [
        "vulnerability-class",
        "auth/authorization",
        "SSRF/outbound",
        "container/sandbox/runtime",
        "multi-tenant isolation",
    ] {
        assert!(
            protocol.contains(phrase) || config.system_prompt.contains(phrase),
            "security prompts missing taxonomy phrase {phrase}"
        );
    }
}

fn security_test_finding(
    id: &str,
    title: &str,
    root_cause: &str,
) -> security_engineer::SecurityFinding {
    security_engineer::SecurityFinding {
        id: id.into(),
        title: title.into(),
        severity: "medium".into(),
        vulnerability_class: "auth_authorization".into(),
        trust_boundary: "HTTP auth boundary".into(),
        entry_point: "src/lib.rs:1".into(),
        sink_or_decision: "authorization decision".into(),
        root_cause: root_cause.into(),
        affected_paths: vec!["src/lib.rs:1".into()],
        evidence: vec!["read_file src/lib.rs:1".into()],
        reachability: "known reachable".into(),
        tenant_or_instance_impact: "cross-tenant access possible".into(),
        severity_rationale: "medium because the route crosses an auth boundary".into(),
        fix_recommendation: "add an owner-scoped authorization check".into(),
    }
}

fn security_test_checkpoint() -> security_engineer::SecurityCheckpoint {
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "sec-test".into(),
        security_engineer::TargetRef {
            repo_path: "/repo".into(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint
        .class_coverage
        .push(security_engineer::VulnerabilityClassCoverage {
            class_id: "auth_authorization".into(),
            class_name: "Authentication and authorization".into(),
            considered: true,
            applicable: true,
            hunted: true,
            ..Default::default()
        });
    checkpoint
}

fn confirmed_decision(id: &str) -> security_engineer::ValidationDecision {
    security_engineer::ValidationDecision {
        finding_id: id.into(),
        decision: security_engineer::ValidationDecisionKind::Confirmed,
        evidence: "validator reproduced the issue".into(),
        severity: Some("medium".into()),
    }
}

#[test]
fn security_engineer_report_prompts_require_root_cause() {
    let report = include_str!("prompts/security_engineer_report.md");
    let repair = include_str!("prompts/security_engineer_report_repair.md");
    for prompt in [report, repair] {
        let lower = prompt.to_ascii_lowercase();
        assert!(prompt.contains("root_cause"));
        assert!(lower.contains("every finding"));
        assert!(lower.contains("dedupe group"));
    }
}

#[test]
fn security_engineer_validator_output_cannot_emit_new_findings() {
    let findings = vec![security_engineer::SecurityFinding {
        id: "finding-001".into(),
        title: "title".into(),
        severity: "medium".into(),
        vulnerability_class: "auth_authorization".into(),
        trust_boundary: "HTTP auth boundary".into(),
        entry_point: "src/lib.rs:1".into(),
        sink_or_decision: "authorization decision".into(),
        root_cause: "root".into(),
        affected_paths: vec!["src/lib.rs:1".into()],
        evidence: vec!["evidence".into()],
        reachability: "not traced".into(),
        tenant_or_instance_impact: "none".into(),
        severity_rationale: "medium because reachability is not traced".into(),
        fix_recommendation: "add explicit authorization check".into(),
    }];
    let raw = r#"{
      "findings": [{"id": "finding-002"}],
      "decisions": [{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]
    }"#;
    let err = security_engineer::parse_validate_output(raw, &findings).unwrap_err();
    assert!(err.contains("must not include new findings"));
}

#[test]
fn security_engineer_validator_rejects_unknown_finding_id() {
    let findings = vec![security_engineer::SecurityFinding {
        id: "finding-001".into(),
        title: "title".into(),
        severity: "medium".into(),
        vulnerability_class: "auth_authorization".into(),
        trust_boundary: "HTTP auth boundary".into(),
        entry_point: "src/lib.rs:1".into(),
        sink_or_decision: "authorization decision".into(),
        root_cause: "root".into(),
        affected_paths: vec![],
        evidence: vec![],
        reachability: "not traced".into(),
        tenant_or_instance_impact: "none".into(),
        severity_rationale: "medium because reachability is not traced".into(),
        fix_recommendation: "add explicit authorization check".into(),
    }];
    let raw =
        r#"{"decisions":[{"finding_id":"finding-999","decision":"confirmed","evidence":"no"}]}"#;
    let err = security_engineer::parse_validate_output(raw, &findings).unwrap_err();
    assert!(err.contains("unknown finding_id"));
}

#[test]
fn security_engineer_validator_cannot_confirm_without_root_cause() {
    let findings = vec![security_test_finding(
        "finding-001",
        "missing owner check",
        "",
    )];
    let raw =
        r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
    let err = security_engineer::parse_validate_output(raw, &findings).unwrap_err();
    assert!(err.contains("cannot confirm"));
    assert!(err.contains("root_cause"));
}

#[test]
fn security_engineer_validator_cannot_confirm_no_vulnerability_notes() {
    let findings = vec![security_test_finding(
        "finding-001",
        "Login redirect and CSRF protections verified -- no bypass found",
        "no vulnerability found in the tested redirect and CSRF boundary",
    )];
    let raw =
        r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
    let err = security_engineer::parse_validate_output(raw, &findings).unwrap_err();
    assert!(err.contains("no-vulnerability verification note"));
}

#[test]
fn security_engineer_report_schema_rejects_malformed_reports() {
    let malformed = serde_json::json!({
        "schema_version": 1,
        "run_id": "",
        "target": {"repo_path": ""},
        "scope": "auth",
        "findings": []
    });
    let err = security_engineer::validate_report_json(&malformed).unwrap_err();
    assert!(err.contains("run_id"));
}

#[test]
fn security_engineer_report_error_identifies_missing_root_cause_item() {
    let mut checkpoint = security_test_checkpoint();
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-001",
        "missing owner check",
        "owner predicate absent",
    ));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-001"));
    let mut value = serde_json::to_value(security_engineer::report_from_checkpoint(&checkpoint))
        .expect("report serializes");
    value["findings"][0]
        .as_object_mut()
        .unwrap()
        .remove("root_cause");
    let err = security_engineer::validate_report_json(&value).unwrap_err();
    assert!(err.contains("findings[0]"));
    assert!(err.contains("finding-001"));
    assert!(err.contains("missing required field root_cause"));

    let mut value = serde_json::to_value(security_engineer::report_from_checkpoint(&checkpoint))
        .expect("report serializes");
    value["dedupe_groups"][0]
        .as_object_mut()
        .unwrap()
        .remove("root_cause");
    let err = security_engineer::validate_report_json(&value).unwrap_err();
    assert!(err.contains("dedupe_groups[0]"));
    assert!(err.contains("dedupe-001"));
    assert!(err.contains("missing required field root_cause"));
}

#[test]
fn security_engineer_report_repair_succeeds_after_truncated_report() {
    let mut checkpoint = security_test_checkpoint();
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-001",
        "missing owner check",
        "owner predicate absent",
    ));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-001"));
    let first_err = security_engineer::parse_report_output("{").unwrap_err();
    let repair_raw =
        serde_json::to_string(&security_engineer::report_from_checkpoint(&checkpoint)).unwrap();
    let (report, state) = security_engineer::resolve_repaired_or_fallback_report(
        &checkpoint,
        &first_err,
        &repair_raw,
    )
    .unwrap();
    assert_eq!(state.status, "valid");
    assert!(state.errors.is_empty());
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].root_cause, "owner predicate absent");
}

#[test]
fn security_engineer_deterministic_fallback_succeeds_when_report_and_repair_are_malformed() {
    let mut checkpoint = security_test_checkpoint();
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-001",
        "missing owner check",
        "owner predicate absent",
    ));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-001"));
    let first_err = security_engineer::parse_report_output("{").unwrap_err();
    let (report, state) =
        security_engineer::resolve_repaired_or_fallback_report(&checkpoint, &first_err, "{")
            .unwrap();
    assert_eq!(state.status, "deterministic_fallback");
    assert_eq!(state.errors.len(), 2);
    assert_eq!(report.findings.len(), 1);
}

#[test]
fn security_engineer_report_from_checkpoint_includes_only_reportable_confirmed_findings() {
    let mut checkpoint = security_test_checkpoint();
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-001",
        "missing owner check",
        "owner predicate absent",
    ));
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-002",
        "Login redirect and CSRF protections verified -- no bypass found",
        "no vulnerability found in the tested redirect and CSRF boundary",
    ));
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-003",
        "missing root cause",
        "",
    ));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-001"));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-002"));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-003"));

    let report = security_engineer::report_from_checkpoint(&checkpoint);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].id, "finding-001");
    assert_eq!(report.dedupe_groups.len(), 1);
    assert_eq!(report.dedupe_groups[0].finding_ids, vec!["finding-001"]);
}

#[test]
fn security_engineer_dedupe_stage_uses_only_reportable_confirmed_findings() {
    let mut checkpoint = security_test_checkpoint();
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-001",
        "missing owner check",
        "shared root cause",
    ));
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-002",
        "Provider adapter registry has no unknown-provider fallback -- verified safe",
        "no vulnerability found in provider fallback handling",
    ));
    checkpoint.findings_so_far.push(security_test_finding(
        "finding-003",
        "unconfirmed matching root",
        "shared root cause",
    ));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-001"));
    checkpoint
        .validation_decisions_so_far
        .push(confirmed_decision("finding-002"));

    security_engineer::run_dedupe_stage(&mut checkpoint);
    assert_eq!(checkpoint.dedupe_groups_so_far.len(), 1);
    assert_eq!(
        checkpoint.dedupe_groups_so_far[0].finding_ids,
        vec!["finding-001"]
    );
}

#[test]
fn security_engineer_report_schema_requires_class_and_trust_boundary() {
    let malformed = serde_json::json!({
        "schema_version": 1,
        "run_id": "sec-test",
        "target": {"repo_path": "/repo", "git_ref": null},
        "scope": "auth",
        "findings": [{
            "id": "finding-001",
            "title": "missing owner check",
            "severity": "high",
            "root_cause": "owner predicate absent",
            "affected_paths": ["src/proxy.rs:10"],
            "evidence": ["read_file src/proxy.rs:10"],
            "reachability": "known reachable",
            "severity_rationale": "reachable auth boundary",
            "fix_recommendation": "add owner check"
        }],
        "rejected_candidates": [],
        "coverage": [],
        "gaps": [],
        "dedupe_groups": [],
        "trace_evidence": [],
        "stage_history": [],
        "class_coverage": [{
            "class_id": "auth_authorization",
            "class_name": "Authentication and authorization",
            "considered": true,
            "applicable": true,
            "hunted": true
        }]
    });
    let err = security_engineer::validate_report_json(&malformed).unwrap_err();
    assert!(err.contains("vulnerability_class"));
    assert!(err.contains("trust_boundary"));
}

#[test]
fn security_engineer_validator_cannot_confirm_without_required_evidence_fields() {
    let findings = vec![security_engineer::SecurityFinding {
        id: "finding-001".into(),
        title: "thin candidate".into(),
        severity: "high".into(),
        vulnerability_class: String::new(),
        trust_boundary: String::new(),
        entry_point: String::new(),
        sink_or_decision: String::new(),
        root_cause: "missing predicate".into(),
        affected_paths: vec!["src/proxy.rs:10".into()],
        evidence: vec!["read_file evidence".into()],
        reachability: "not traced".into(),
        tenant_or_instance_impact: String::new(),
        severity_rationale: String::new(),
        fix_recommendation: String::new(),
    }];
    let raw =
        r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"ok"}]}"#;
    let err = security_engineer::parse_validate_output(raw, &findings).unwrap_err();
    assert!(err.contains("cannot confirm"));
    assert!(err.contains("vulnerability_class"));
    assert!(err.contains("trust_boundary"));
}

#[tokio::test]
async fn security_engineer_writes_checkpoint_after_recon() {
    let llm = MockLlm::new(vec![mock_text_response(
        r#"{
          "architecture_context": "Rust HTTP boundary",
          "tasks": [
            {"id":"hunt-001","attack_class":"auth_bypass","scope_hint":"src/http","rationale":"proxy auth"}
          ],
	          "coverage_gaps": [],
	          "class_coverage": [
	            {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":false,"task_ids":["hunt-001"]}
	          ]
        }"#,
    )]);
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({
                "task": "review auth boundary",
                "stop_after_stage": "recon"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("checkpoint saved after recon"));

    let guard = workspace.read().await;
    let names = guard.list_files();
    let path = names
        .iter()
        .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
        .expect("checkpoint file not written");
    let body = guard.get(path).unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    assert_eq!(
        checkpoint.current_stage,
        security_engineer::SecurityHarnessStage::Recon
    );
    // Every taxonomy class is queued as a hunt task unconditionally (see
    // ensure_taxonomy_hunt_tasks). The recon-supplied task for
    // auth_authorization keeps its `hunt-001` id; the other classes get
    // generic taxonomy-driven tasks queued after it.
    assert_eq!(
        checkpoint.pending_tasks.len(),
        security_engineer::vulnerability_taxonomy().len()
    );
    assert!(
        checkpoint
            .pending_tasks
            .iter()
            .any(|task| task.id == "hunt-001" && task.attack_class == "auth_authorization"),
        "recon-supplied auth_authorization task should survive as hunt-001"
    );
    assert!(
        checkpoint
            .class_coverage
            .iter()
            .any(|class| class.class_id == "auth_authorization"
                && class.considered
                && class.applicable
                && class.task_ids.iter().any(|id| id == "hunt-001")),
        "checkpoint should record taxonomy coverage"
    );
    assert_eq!(
        checkpoint.stage_history[0].stage,
        security_engineer::SecurityHarnessStage::Recon
    );
}

#[tokio::test]
async fn security_engineer_recon_proceeds_when_subagent_returns_prose() {
    // Live failure on deepseek-v4-pro c-0050: the recon child blew through
    // its 12-iteration cap, the agent loop's summarize-on-cap path returned
    // a prose summary, and parse_stage_json failed. The whole harness
    // returned Err to TARS, who fell back to manual review and produced a
    // report titled "security_engineer subagent unavailable" — silently
    // killing the recon→hunt transition.
    //
    // After the fix, recon parse failure is non-fatal: the taxonomy fan-out
    // still queues every class, the checkpoint is still written, and a
    // resume will pick it up at the hunt stage.
    let llm = MockLlm::new(vec![mock_text_response(
        "I have reached the maximum number of iterations. Here is what I found: \
         the codebase has an HTTP server, some auth code, and a few config files. \
         I did not have time to enumerate every entry point.",
    )]);
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({
                "task": "review iteration-cap regression",
                "stop_after_stage": "recon"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "recon should succeed even when the subagent returns prose, got: {}",
        result.content
    );

    let guard = workspace.read().await;
    let path = guard
        .list_files()
        .into_iter()
        .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
        .expect("checkpoint file not written on prose-output recon");
    let body = guard.get(&path).unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    // architecture_context stayed empty (recon defaulted), but hunt tasks
    // for every taxonomy class are still queued.
    assert!(checkpoint.architecture_context.is_empty());
    assert_eq!(
        checkpoint.pending_tasks.len(),
        security_engineer::vulnerability_taxonomy().len(),
        "every taxonomy class should be queued even after recon parse failure"
    );
}

#[tokio::test]
async fn security_engineer_recon_generates_taxonomy_driven_tasks() {
    let llm = MockLlm::new(vec![mock_text_response(
        r#"{
          "architecture_context": "MCP proxy runtime with Docker containers, bearer tokens, OAuth callback, owner instance checks, outbound URL policy, JSON body caps, and frontend secret reveal UI",
          "tasks": [],
          "coverage_gaps": [],
          "class_coverage": []
        }"#,
    )]);
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({
                "task": "review MCP runtime proxy boundary",
                "stop_after_stage": "recon"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);

    let guard = workspace.read().await;
    let path = guard
        .list_files()
        .into_iter()
        .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
        .expect("checkpoint file not written");
    let body = guard.get(&path).unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    let task_classes: std::collections::BTreeSet<_> = checkpoint
        .pending_tasks
        .iter()
        .map(|task| task.attack_class.as_str())
        .collect();
    for expected in [
        "auth_authorization",
        "session_oauth_csrf",
        "ssrf_outbound_network",
        "proxy_http_boundary",
        "container_sandbox_runtime",
        "secrets_credentials",
        "multi_tenant_isolation",
        "resource_exhaustion_dos",
        "frontend_security_ux",
        "agent_tool_boundary",
        "api_contract_input_validation",
    ] {
        assert!(
            task_classes.contains(expected),
            "missing generated task for {expected}"
        );
    }
    assert!(
        checkpoint
            .class_coverage
            .iter()
            .filter(|class| class.considered)
            .count()
            >= security_engineer::vulnerability_taxonomy().len()
    );
}

#[tokio::test]
async fn security_engineer_resumes_checkpoint_and_does_not_rerun_completed_tasks() {
    let report_json = r#"{
      "schema_version": 1,
      "run_id": "placeholder",
      "target": {"repo_path": "placeholder", "git_ref": null},
      "scope": "placeholder",
      "findings": [],
      "rejected_candidates": [],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [],
      "trace_evidence": [],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":false,"task_ids":["hunt-001"]}
      ]
    }"#;
    // First run gets recon + the auth_authorization hunt-finding response.
    // Every other taxonomy class hunts in parallel through the fallback.
    let first_llm = MockLlm::new(vec![
        mock_text_response(
            r#"{
              "architecture_context": "proxy boundary",
              "tasks": [
                {"id":"hunt-001","attack_class":"auth_bypass","scope_hint":"proxy","rationale":"bearer token"}
              ],
	              "coverage_gaps": [],
	              "class_coverage": [
	                {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":false,"task_ids":["hunt-001"]},
	                {"class_id":"proxy_http_boundary","class_name":"Proxy and HTTP boundary issues","considered":true,"applicable":false,"hunted":false,"skipped_reason":"not part of this resume regression"},
	                {"class_id":"ssrf_outbound_network","class_name":"SSRF and outbound network policy","considered":true,"applicable":false,"hunted":false,"skipped_reason":"not part of this resume regression"}
	              ]
            }"#,
        ),
        mock_text_response(
            r#"{
              "completed_task_ids": ["hunt-001"],
              "findings": [
                {
	                  "id":"finding-001",
	                  "title":"candidate",
	                  "severity":"medium",
	                  "vulnerability_class":"auth_authorization",
	                  "trust_boundary":"proxy bearer boundary",
	                  "entry_point":"src/proxy.rs:10",
	                  "sink_or_decision":"proxy authorization decision",
	                  "root_cause":"boundary confusion",
	                  "affected_paths":["src/proxy.rs:10"],
	                  "evidence":["read_file evidence"],
	                  "reachability":"not traced",
	                  "tenant_or_instance_impact":"possible cross-instance access",
	                  "severity_rationale":"medium until reachability is traced",
	                  "fix_recommendation":"resolve instance ownership before proxying"
                }
              ],
              "gaps": [],
              "follow_up_tasks": []
            }"#,
        ),
    ])
    .with_fallback(
        r#"{"completed_task_ids": [], "findings": [], "gaps": [], "follow_up_tasks": []}"#,
    );
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let first_tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(first_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));

    let first = first_tool
        .run(
            &serde_json::json!({
                "task": "review proxy boundary",
                "stop_after_stage": "hunt"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!first.is_error, "{}", first.content);
    let run_id = {
        let guard = workspace.read().await;
        let path = guard
            .list_files()
            .into_iter()
            .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
            .unwrap();
        let body = guard.get(&path).unwrap();
        let checkpoint: security_engineer::SecurityCheckpoint =
            serde_json::from_str(&body).unwrap();
        // Every taxonomy class is hunted unconditionally now, so the
        // first run completes one task per class. The point of this
        // assertion is "the auth_authorization hunt ran" — verify that
        // explicitly so the test stays meaningful as the taxonomy grows.
        assert!(checkpoint.completed_tasks.len() >= 1);
        assert!(
            checkpoint
                .completed_tasks
                .iter()
                .any(|task| task.id == "hunt-001"),
            "expected hunt-001 to be among completed tasks"
        );
        checkpoint.run_id
    };

    // Second run: validate + trace + report. A fresh MockLlm with no
    // fallback keeps the test honest — if the resume erroneously re-runs
    // a completed hunt task, the validate-shaped JSON would be consumed
    // by the hunt parser and the test would panic on queue exhaustion.
    let resume_llm = MockLlm::new(vec![
        mock_text_response(
            r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"still reachable","severity":"medium"}]}"#,
        ),
        mock_text_response(
            r#"{"traces":[{"finding_id":"finding-001","reachable":true,"severity_effect":"keeps","evidence":["trace evidence"]}]}"#,
        ),
        mock_text_response(report_json),
    ]);
    let resume_tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(resume_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let resumed = resume_tool
        .run(
            &serde_json::json!({
                "task": "resume security review",
                "resume": true,
                "run_id": run_id
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!resumed.is_error, "{}", resumed.content);
    assert!(resumed.content.contains("# Security Harness Report"));
    assert!(resumed.content.contains("## Findings"));
    assert!(resumed.content.contains("candidate"));
    assert!(!resumed.content.contains("```json"));
    assert_eq!(resumed.artefacts.len(), 1);
    assert!(resumed.artefacts[0].content.contains("## Findings"));
    assert!(!resumed.artefacts[0].content.contains("```json"));

    let guard = workspace.read().await;
    let path = guard
        .list_files()
        .into_iter()
        .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
        .unwrap();
    let body = guard.get(&path).unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    assert!(checkpoint.completed);
    // Every taxonomy class is hunted unconditionally, so the final
    // checkpoint includes one completed task per class. Verify the
    // recon-supplied auth_authorization hunt (`hunt-001`) is among them
    // rather than asserting an exact count.
    assert!(checkpoint.completed_tasks.len() >= 1);
    assert!(
        checkpoint
            .completed_tasks
            .iter()
            .any(|task| task.id == "hunt-001"),
        "expected hunt-001 to be among completed tasks"
    );
    assert_eq!(checkpoint.validation_decisions_so_far.len(), 1);
    assert!(
        checkpoint
            .stage_history
            .iter()
            .any(|entry| entry.stage == security_engineer::SecurityHarnessStage::Validate)
    );
}

#[tokio::test]
async fn security_engineer_resumes_json_checkpoint_after_filesystem_workspace_reload() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(
            crate::workspace::FilesystemWorkspace::load(
                workspace_dir.path(),
                crate::config::MemoryConfig::default(),
            )
            .unwrap(),
        ) as Box<dyn crate::workspace::Workspace>,
    ));
    let first_llm = MockLlm::new(vec![
        mock_text_response(
            r#"{
              "architecture_context": "runtime socket boundary",
              "tasks": [
	                {"id":"hunt-001","attack_class":"container_sandbox_runtime","scope_hint":"runtime","rationale":"socket auth"}
              ],
	              "coverage_gaps": [],
	              "class_coverage": [
	                {"class_id":"container_sandbox_runtime","class_name":"Container, sandbox, and runtime escape","considered":true,"applicable":true,"hunted":false,"task_ids":["hunt-001"]}
	              ]
            }"#,
        ),
        mock_text_response(
            r#"{
              "completed_task_ids": ["hunt-001"],
              "findings": [
                {
	                  "id":"finding-001",
	                  "title":"socket identity missing",
	                  "severity":"high",
	                  "vulnerability_class":"container_sandbox_runtime",
	                  "trust_boundary":"runtime Unix socket boundary",
	                  "entry_point":"crates/mcp-runtime/src/main.rs:1",
	                  "sink_or_decision":"runtime forward decision",
	                  "root_cause":"missing caller identity",
	                  "affected_paths":["crates/mcp-runtime/src/main.rs:1"],
	                  "evidence":["runtime socket evidence"],
	                  "reachability":"not traced",
	                  "tenant_or_instance_impact":"one instance could affect another runtime server",
	                  "severity_rationale":"high because runtime forwarding crosses the sandbox boundary",
	                  "fix_recommendation":"authenticate runtime socket requests with instance identity"
                }
              ],
              "gaps": [],
              "follow_up_tasks": []
            }"#,
        ),
    ])
    .with_fallback(
        r#"{"completed_task_ids": [], "findings": [], "gaps": [], "follow_up_tasks": []}"#,
    );
    let first_tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(first_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(workspace_dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let first = first_tool
        .run(
            &serde_json::json!({
                "task": "review runtime boundary",
                "stop_after_stage": "hunt"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!first.is_error, "{}", first.content);
    let run_id = {
        let guard = workspace.read().await;
        let path = guard
            .list_files()
            .into_iter()
            .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
            .unwrap();
        let body = guard.get(&path).unwrap();
        let checkpoint: security_engineer::SecurityCheckpoint =
            serde_json::from_str(&body).unwrap();
        checkpoint.run_id
    };
    let checkpoint_rel = format!("kb/security-harness/checkpoints/{run_id}.json");
    assert!(
        workspace_dir.path().join(&checkpoint_rel).is_file(),
        "checkpoint must be persisted to the filesystem"
    );

    let reloaded_workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(
            crate::workspace::FilesystemWorkspace::load(
                workspace_dir.path(),
                crate::config::MemoryConfig::default(),
            )
            .unwrap(),
        ) as Box<dyn crate::workspace::Workspace>,
    ));
    {
        let guard = reloaded_workspace.read().await;
        assert!(
            guard.get(&checkpoint_rel).is_none(),
            "filesystem workspace indexes markdown kb files, not checkpoint JSON"
        );
    }

    let report_json = r#"{
      "schema_version": 1,
      "run_id": "resumed",
      "target": {"repo_path": "placeholder", "git_ref": null},
      "scope": "placeholder",
      "findings": [],
      "rejected_candidates": [],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [],
      "trace_evidence": [],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"container_sandbox_runtime","class_name":"Container, sandbox, and runtime escape","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":false,"task_ids":["hunt-001"]}
      ]
    }"#;
    let resume_llm = MockLlm::new(vec![
        mock_text_response(
            r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"checkpoint loaded","severity":"high"}]}"#,
        ),
        mock_text_response(
            r#"{"traces":[{"finding_id":"finding-001","reachable":true,"severity_effect":"keeps","evidence":["trace evidence"]}]}"#,
        ),
        mock_text_response(report_json),
    ]);
    let resume_tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(resume_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&reloaded_workspace)),
        &[],
        vec![],
    );
    let mut resume_ctx = ToolContext::for_test(workspace_dir.path());
    resume_ctx.workspace = Some(Arc::clone(&reloaded_workspace));
    let resumed = resume_tool
        .run(
            &serde_json::json!({
                "task": "resume security review",
                "resume": true,
                "run_id": run_id
            }),
            &resume_ctx,
        )
        .await
        .unwrap();
    assert!(!resumed.is_error, "{}", resumed.content);
    assert!(resumed.content.contains("# Security Harness Report"));
}

#[tokio::test]
async fn security_engineer_trace_parse_failure_records_gap_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "trace-nonjson".into(),
        security_engineer::TargetRef {
            repo_path: dir.path().display().to_string(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint.current_stage = security_engineer::SecurityHarnessStage::Trace;
    checkpoint
        .completed_tasks
        .push(security_engineer::SecurityTask {
            id: "hunt-001".into(),
            attack_class: "auth_bypass".into(),
            scope_hint: "runtime".into(),
            status: security_engineer::TaskStatus::Completed,
            rationale: "done".into(),
        });
    checkpoint
        .findings_so_far
        .push(security_engineer::SecurityFinding {
            id: "finding-001".into(),
            title: "missing auth".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "runtime socket boundary".into(),
            entry_point: "src/main.rs:1".into(),
            sink_or_decision: "authorization decision".into(),
            root_cause: "root".into(),
            affected_paths: vec!["src/main.rs:1".into()],
            evidence: vec!["evidence".into()],
            reachability: "not traced".into(),
            tenant_or_instance_impact: "instance crossover possible".into(),
            severity_rationale: "high because a reachable boundary lacks caller identity".into(),
            fix_recommendation: "bind runtime socket calls to resolved instance identity".into(),
        });
    checkpoint
        .validation_decisions_so_far
        .push(security_engineer::ValidationDecision {
            finding_id: "finding-001".into(),
            decision: security_engineer::ValidationDecisionKind::Confirmed,
            evidence: "confirmed".into(),
            severity: Some("high".into()),
        });
    checkpoint
        .dedupe_groups_so_far
        .push(security_engineer::DedupeGroup {
            id: "dedupe-001".into(),
            root_cause: "root".into(),
            primary_finding_id: "finding-001".into(),
            finding_ids: vec!["finding-001".into()],
            affected_paths: vec!["src/main.rs:1".into()],
        });
    for stage in [
        security_engineer::SecurityHarnessStage::Recon,
        security_engineer::SecurityHarnessStage::Hunt,
        security_engineer::SecurityHarnessStage::Validate,
        security_engineer::SecurityHarnessStage::Gapfill,
        security_engineer::SecurityHarnessStage::Dedupe,
    ] {
        checkpoint
            .stage_history
            .push(security_engineer::StageHistoryEntry {
                stage,
                status: "completed".into(),
                started_at: 1,
                finished_at: 2,
                summary: "done".into(),
            });
    }

    let body = serde_json::to_string_pretty(&checkpoint).unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(
            crate::workspace::InMemoryWorkspace::new()
                .with_file("kb/security-harness/checkpoints/trace-nonjson.json", &body),
        ) as Box<dyn crate::workspace::Workspace>,
    ));
    let report_json = r#"{
      "schema_version": 1,
      "run_id": "trace-nonjson",
      "target": {"repo_path": "repo", "git_ref": null},
      "scope": "scope",
      "findings": [],
      "rejected_candidates": [],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [],
      "trace_evidence": [],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":false,"task_ids":["hunt-001"]}
      ]
    }"#;
    let llm = MockLlm::new(vec![
        mock_text_response("Trace could not produce machine-readable JSON."),
        mock_text_response(report_json),
    ]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({
                "resume": true,
                "run_id": "trace-nonjson"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("# Security Harness Report"));

    let guard = workspace.read().await;
    let body = guard
        .get("kb/security-harness/checkpoints/trace-nonjson.json")
        .unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    assert!(
        checkpoint
            .coverage_gaps
            .iter()
            .any(|gap| gap.area == "Trace stage")
    );
    assert!(checkpoint.completed);
}

#[tokio::test]
async fn security_engineer_old_checkpoint_fails_safely() {
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "bad".into(),
        security_engineer::TargetRef {
            repo_path: std::env::temp_dir().display().to_string(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint.schema_version = 0;
    let body = serde_json::to_string(&checkpoint).unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(
            crate::workspace::InMemoryWorkspace::new()
                .with_file("kb/security-harness/checkpoints/bad.json", &body),
        ) as Box<dyn crate::workspace::Workspace>,
    ));
    let llm = MockLlm::new(vec![]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(std::path::Path::new(&checkpoint.target.repo_path));
    ctx.workspace = Some(workspace);
    let result = tool
        .run(
            &serde_json::json!({
                "resume": true,
                "run_id": "bad"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("unsupported checkpoint schema_version")
    );
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    let config = OrchestratorConfig {
        name: "test_orchestrator",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &[],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        emit_artefact: None,
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        sandbox_bypass: None,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
        tool_use_id: None,
        subagent_events: None,
        artefacts: None,
        current_chat_id: None,
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

    let config = OrchestratorConfig {
        name: "test_orchestrator",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &[],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        emit_artefact: None,
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    assert_eq!(
        result.content,
        "Security review complete. No critical issues found."
    );
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

    let config = OrchestratorConfig {
        name: "security_engineer",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &[],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        emit_artefact: Some(crate::message::ArtefactKind::SecurityReview),
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    assert!(matches!(
        art.kind,
        crate::message::ArtefactKind::SecurityReview
    ));
    assert_eq!(
        art.content,
        "Security review complete. No critical issues found."
    );
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

    let config = OrchestratorConfig {
        name: "security_engineer",
        description: "test",
        system_prompt: "test",
        direct_tool_names: &[],
        max_iterations: 5,
        max_tokens: 1024,
        injects_protocol: None,
        emit_artefact: Some(crate::message::ArtefactKind::SecurityReview),
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        None,
        &[],
        vec![],
    );

    let ctx = ToolContext::from_cwd().unwrap();
    let input = serde_json::json!({ "task": "Review the module" });
    let result = tool.run(&input, &ctx).await.unwrap();
    assert!(
        result.artefacts.is_empty(),
        "whitespace-only content should not emit"
    );
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
        emit_artefact: None,
        harness: None,
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    let file = std::env::temp_dir().join(format!("dyson-orch-test-{}.tmp", std::process::id()));
    std::fs::write(&file, b"not a directory").unwrap();

    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(crate::llm::create_client(
            &crate::config::AgentSettings::default(),
            None,
            None,
        )),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
    async fn run(&self, _input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
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
        emit_artefact: None,
        harness: None,
    };

    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
        sandbox_bypass: None,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        activity: None,
        tool_use_id: None,
        subagent_events: None,
        artefacts: None,
        current_chat_id: None,
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
        captured_dir,
        scoped,
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
        emit_artefact: None,
        harness: None,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
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
// security_engineer end-to-end simulated runs.
//
// Each test below mocks one or more stage outputs with realistic JSON
// payloads, drives the harness through one or more stages (sometimes via
// `stop_after_stage`, sometimes via a pre-seeded checkpoint), and asserts
// about the resulting tool output + the durable checkpoint.
//
// MockLlm uses `.with_fallback(...)` for the unconditional hunt fan-out:
// the harness spawns one specialist per taxonomy class, and we only
// enumerate the responses for the specialists that matter to the test.
// -----------------------------------------------------------------------

/// Realistic recon-stage output emitting one finding-worthy task.
fn e2e_recon_response_for_auth() -> Vec<crate::llm::stream::StreamEvent> {
    mock_text_response(
        r#"{
              "architecture_context": "HTTP proxy with bearer tokens",
              "tasks": [
                {"id":"hunt-auth-001","attack_class":"auth_authorization","scope_hint":"src/proxy.rs","rationale":"bearer token boundary"}
              ],
              "coverage_gaps": [],
              "class_coverage": [
                {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":false,"task_ids":["hunt-auth-001"]}
              ]
            }"#,
    )
}

/// Realistic hunt-stage output reporting one complete finding.
fn e2e_hunt_finding_response() -> Vec<crate::llm::stream::StreamEvent> {
    mock_text_response(
        r#"{
              "completed_task_ids": ["hunt-auth-001"],
              "findings": [
                {
                  "id":"finding-001",
                  "title":"missing owner predicate on /api/items",
                  "severity":"high",
                  "vulnerability_class":"auth_authorization",
                  "trust_boundary":"HTTP request boundary",
                  "entry_point":"src/proxy.rs:42",
                  "sink_or_decision":"store.fetch_by_id authorization gate",
                  "root_cause":"owner predicate missing from id-based fetch",
                  "affected_paths":["src/proxy.rs:42"],
                  "evidence":["read_file src/proxy.rs:42 — no owner predicate visible","taint_trace req.params.id -> store.fetch_by_id"],
                  "reachability":"unauthenticated GET",
                  "tenant_or_instance_impact":"cross-tenant data access",
                  "severity_rationale":"high because reachable without auth",
                  "fix_recommendation":"add owner_id predicate to fetch_by_id"
                }
              ],
              "gaps": [],
              "follow_up_tasks": []
            }"#,
    )
}

fn e2e_hunt_empty_fallback() -> &'static str {
    r#"{"completed_task_ids": [], "findings": [], "gaps": [], "follow_up_tasks": []}"#
}

fn e2e_validate_confirm_response() -> Vec<crate::llm::stream::StreamEvent> {
    mock_text_response(
        r#"{"decisions":[{"finding_id":"finding-001","decision":"confirmed","evidence":"validator reproduced the bypass","severity":"high"}]}"#,
    )
}

fn e2e_validate_reject_response() -> Vec<crate::llm::stream::StreamEvent> {
    mock_text_response(
        r#"{"decisions":[{"finding_id":"finding-001","decision":"rejected","evidence":"validator found an upstream auth check that hunters missed","severity":"info"}]}"#,
    )
}

fn e2e_trace_reachable_response() -> Vec<crate::llm::stream::StreamEvent> {
    mock_text_response(
        r#"{"traces":[{"finding_id":"finding-001","reachable":true,"severity_effect":"keeps","evidence":["taint_trace req -> sink"]}]}"#,
    )
}

fn e2e_full_report_json() -> &'static str {
    r#"{
      "schema_version": 1,
      "run_id": "placeholder",
      "target": {"repo_path": "placeholder", "git_ref": null},
      "scope": "placeholder",
      "findings": [
        {
          "id":"finding-001",
          "title":"missing owner predicate on /api/items",
          "severity":"high",
          "vulnerability_class":"auth_authorization",
          "trust_boundary":"HTTP request boundary",
          "entry_point":"src/proxy.rs:42",
          "sink_or_decision":"store.fetch_by_id authorization gate",
          "root_cause":"owner predicate missing from id-based fetch",
          "affected_paths":["src/proxy.rs:42"],
          "evidence":["read_file src/proxy.rs:42 — no owner predicate visible","taint_trace req.params.id -> store.fetch_by_id"],
          "reachability":"unauthenticated GET",
          "tenant_or_instance_impact":"cross-tenant data access",
          "severity_rationale":"high because reachable without auth",
          "fix_recommendation":"add owner_id predicate to fetch_by_id"
        }
      ],
      "rejected_candidates": [],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [
        {"id":"dedupe-001","root_cause":"owner predicate missing from id-based fetch","primary_finding_id":"finding-001","finding_ids":["finding-001"],"affected_paths":["src/proxy.rs:42"]}
      ],
      "trace_evidence": [{"finding_id":"finding-001","reachable":true,"severity_effect":"keeps","evidence":["taint_trace req -> sink"]}],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":false,"task_ids":["hunt-auth-001"]}
      ]
    }"#
}

fn e2e_empty_report_json() -> &'static str {
    r#"{
      "schema_version": 1,
      "run_id": "placeholder",
      "target": {"repo_path": "placeholder", "git_ref": null},
      "scope": "placeholder",
      "findings": [],
      "rejected_candidates": [],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [],
      "trace_evidence": [],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":true}
      ]
    }"#
}

fn e2e_rejected_report_json() -> &'static str {
    r#"{
      "schema_version": 1,
      "run_id": "placeholder",
      "target": {"repo_path": "placeholder", "git_ref": null},
      "scope": "placeholder",
      "findings": [],
      "rejected_candidates": [
        {"finding_id":"finding-001","decision":"rejected","evidence":"upstream auth check exists","severity":"info"}
      ],
      "coverage": [],
      "gaps": [],
      "dedupe_groups": [],
      "trace_evidence": [],
      "stage_history": [],
      "class_coverage": [
        {"class_id":"auth_authorization","class_name":"Authentication and authorization","considered":true,"applicable":true,"hunted":true,"checked_and_cleared":true}
      ]
    }"#
}

/// Run the harness one stage at a time using `stop_after_stage`,
/// rebuilding the tool each time with a fresh MockLlm. This keeps each
/// stage's mocks isolated from the unconditional hunt fan-out — driving
/// the whole pipeline via a single MockLlm queue produces non-determinism
/// because `buffer_unordered` can deliver the validate/trace/report
/// responses to random hunt specialists.
async fn run_stage(
    workspace: &crate::workspace::WorkspaceHandle,
    dir: &std::path::Path,
    run_id: Option<&str>,
    stop_after: &str,
    llm: MockLlm,
) -> ToolOutput {
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir);
    ctx.workspace = Some(Arc::clone(workspace));
    let mut input = serde_json::json!({
        "task": "drive harness one stage",
        "stop_after_stage": stop_after,
    });
    if let Some(id) = run_id {
        input["resume"] = serde_json::Value::Bool(true);
        input["run_id"] = serde_json::Value::String(id.into());
        input["task"] = serde_json::Value::String("resume".into());
    }
    tool.run(&input, &ctx).await.unwrap()
}

async fn load_checkpoint(
    workspace: &crate::workspace::WorkspaceHandle,
) -> security_engineer::SecurityCheckpoint {
    let guard = workspace.read().await;
    let path = guard
        .list_files()
        .into_iter()
        .find(|name| name.starts_with("kb/security-harness/checkpoints/"))
        .expect("checkpoint should be written");
    serde_json::from_str(&guard.get(&path).unwrap()).unwrap()
}

#[tokio::test]
async fn security_engineer_e2e_happy_path_one_finding() {
    // Full 8-stage run, but driven stage-by-stage to keep the
    // unconditional hunt fan-out from eating later stages' mocks. We
    // verify the FINAL stage_history records every stage.
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let dir = tempfile::tempdir().unwrap();

    // Recon stage.
    let recon_llm = MockLlm::new(vec![e2e_recon_response_for_auth()]);
    let out = run_stage(&workspace, dir.path(), None, "recon", recon_llm).await;
    assert!(!out.is_error, "recon stage error: {}", out.content);
    let run_id = load_checkpoint(&workspace).await.run_id;

    // Hunt stage: every taxonomy class fans out as its own specialist.
    // We only care about the auth_authorization specialist; the
    // fallback covers every other class with an empty-findings reply.
    let hunt_llm =
        MockLlm::new(vec![e2e_hunt_finding_response()]).with_fallback(e2e_hunt_empty_fallback());
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "hunt", hunt_llm).await;
    assert!(!out.is_error, "hunt stage error: {}", out.content);

    // Validate stage.
    let validate_llm = MockLlm::new(vec![e2e_validate_confirm_response()]);
    let out = run_stage(
        &workspace,
        dir.path(),
        Some(&run_id),
        "validate",
        validate_llm,
    )
    .await;
    assert!(!out.is_error, "validate stage error: {}", out.content);

    // Gapfill + dedupe are bookkeeping-only (no LLM calls).
    let llm = MockLlm::new(vec![]);
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "gapfill", llm).await;
    assert!(!out.is_error, "gapfill error: {}", out.content);
    let llm = MockLlm::new(vec![]);
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "dedupe", llm).await;
    assert!(!out.is_error, "dedupe error: {}", out.content);

    // Trace stage.
    let trace_llm = MockLlm::new(vec![e2e_trace_reachable_response()]);
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "trace", trace_llm).await;
    assert!(!out.is_error, "trace stage error: {}", out.content);

    // Feedback is bookkeeping-only.
    let llm = MockLlm::new(vec![]);
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "feedback", llm).await;
    assert!(!out.is_error, "feedback error: {}", out.content);

    // Report stage — the final run, no stop_after_stage so the harness
    // completes and emits the rendered report.
    let report_llm = MockLlm::new(vec![mock_text_response(e2e_full_report_json())]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(report_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"task": "resume", "resume": true, "run_id": &run_id}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "report stage error: {}", result.content);
    assert!(
        result.content.contains("# Security Harness Report"),
        "tool output should include the rendered markdown report"
    );
    assert!(
        result.content.contains("finding-001"),
        "rendered report should mention the confirmed finding"
    );

    let checkpoint = load_checkpoint(&workspace).await;
    assert!(checkpoint.completed, "completed flag should be set");
    let stages: std::collections::BTreeSet<_> = checkpoint
        .stage_history
        .iter()
        .map(|entry| entry.stage)
        .collect();
    for stage in [
        security_engineer::SecurityHarnessStage::Recon,
        security_engineer::SecurityHarnessStage::Hunt,
        security_engineer::SecurityHarnessStage::Validate,
        security_engineer::SecurityHarnessStage::Gapfill,
        security_engineer::SecurityHarnessStage::Dedupe,
        security_engineer::SecurityHarnessStage::Trace,
        security_engineer::SecurityHarnessStage::Feedback,
        security_engineer::SecurityHarnessStage::Report,
    ] {
        assert!(
            stages.contains(&stage),
            "stage_history should record completion for {stage}"
        );
    }
}

#[tokio::test]
async fn security_engineer_e2e_recon_prose_still_produces_report() {
    // Recon returns prose (iteration-cap regression path), hunt finds
    // nothing across every taxonomy specialist, but the harness still
    // completes through report with zero findings — no panics, no Err.
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let dir = tempfile::tempdir().unwrap();

    let recon_llm = MockLlm::new(vec![mock_text_response(
        "I exhausted my iteration cap and could not enumerate every entry point.",
    )]);
    let out = run_stage(&workspace, dir.path(), None, "recon", recon_llm).await;
    assert!(
        !out.is_error,
        "recon prose should not be fatal, got: {}",
        out.content
    );
    let run_id = load_checkpoint(&workspace).await.run_id;

    // Hunt: every specialist gets the empty fallback.
    let hunt_llm = MockLlm::new(vec![]).with_fallback(e2e_hunt_empty_fallback());
    let out = run_stage(&workspace, dir.path(), Some(&run_id), "hunt", hunt_llm).await;
    assert!(!out.is_error, "hunt stage error: {}", out.content);

    // Validate isn't reached when there are zero findings — but the
    // harness still records the stage as completed and continues.
    // Just drive through the remaining stages.
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "validate", empty_llm).await;
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "gapfill", empty_llm).await;
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "dedupe", empty_llm).await;
    // Trace is also skipped because there are no confirmed findings.
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "trace", empty_llm).await;
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "feedback", empty_llm).await;

    // Final report stage.
    let report_llm = MockLlm::new(vec![mock_text_response(e2e_empty_report_json())]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(report_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"task": "resume", "resume": true, "run_id": &run_id}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error, "report stage error: {}", result.content);
    assert!(
        result.content.contains("# Security Harness Report"),
        "tool output should include the rendered report"
    );

    let checkpoint = load_checkpoint(&workspace).await;
    assert!(checkpoint.completed, "completed flag should be set");
    assert!(
        checkpoint.findings_so_far.is_empty(),
        "zero hunt findings means findings_so_far should be empty"
    );
}

#[tokio::test]
async fn security_engineer_e2e_hunt_finding_then_rejected_by_validator() {
    // Hunt produces a finding, validate rejects it, the final report
    // has zero confirmed findings, and the rejected one appears in
    // rejected_candidates.
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new())
            as Box<dyn crate::workspace::Workspace>,
    ));
    let dir = tempfile::tempdir().unwrap();

    let recon_llm = MockLlm::new(vec![e2e_recon_response_for_auth()]);
    let _ = run_stage(&workspace, dir.path(), None, "recon", recon_llm).await;
    let run_id = load_checkpoint(&workspace).await.run_id;

    let hunt_llm =
        MockLlm::new(vec![e2e_hunt_finding_response()]).with_fallback(e2e_hunt_empty_fallback());
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "hunt", hunt_llm).await;

    let validate_llm = MockLlm::new(vec![e2e_validate_reject_response()]);
    let _ = run_stage(
        &workspace,
        dir.path(),
        Some(&run_id),
        "validate",
        validate_llm,
    )
    .await;

    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "gapfill", empty_llm).await;
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "dedupe", empty_llm).await;
    // Trace is skipped (no confirmed decisions).
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "trace", empty_llm).await;
    let empty_llm = MockLlm::new(vec![]);
    let _ = run_stage(&workspace, dir.path(), Some(&run_id), "feedback", empty_llm).await;

    // Final report stage.
    let report_llm = MockLlm::new(vec![mock_text_response(e2e_rejected_report_json())]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(report_llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"task": "resume", "resume": true, "run_id": &run_id}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "rejected finding should still complete, got: {}",
        result.content
    );

    let checkpoint = load_checkpoint(&workspace).await;
    assert!(checkpoint.completed, "completed flag should be set");
    let report = checkpoint
        .report_draft
        .as_ref()
        .expect("report_draft should be set after report stage");
    assert!(
        report.findings.is_empty(),
        "rejected finding should not appear in confirmed findings"
    );
    assert_eq!(
        report.rejected_candidates.len(),
        1,
        "rejected candidate should appear in rejected_candidates"
    );
    assert_eq!(report.rejected_candidates[0].finding_id, "finding-001");
}

#[tokio::test]
async fn security_engineer_e2e_report_repair_path() {
    // Pre-seed a checkpoint at the Report stage so we don't have to
    // burn LLM responses on recon/hunt/validate/trace. The report stage
    // first returns malformed JSON, then the repair prompt returns a
    // valid report.
    let dir = tempfile::tempdir().unwrap();
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "e2e-repair".into(),
        security_engineer::TargetRef {
            repo_path: dir.path().display().to_string(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint.current_stage = security_engineer::SecurityHarnessStage::Report;
    checkpoint
        .findings_so_far
        .push(security_engineer::SecurityFinding {
            id: "finding-001".into(),
            title: "missing owner predicate".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "HTTP request boundary".into(),
            entry_point: "src/proxy.rs:42".into(),
            sink_or_decision: "store.fetch_by_id".into(),
            root_cause: "owner predicate missing".into(),
            affected_paths: vec!["src/proxy.rs:42".into()],
            evidence: vec!["read_file evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "cross-tenant access".into(),
            severity_rationale: "high because reachable".into(),
            fix_recommendation: "add owner predicate".into(),
        });
    checkpoint
        .validation_decisions_so_far
        .push(security_engineer::ValidationDecision {
            finding_id: "finding-001".into(),
            decision: security_engineer::ValidationDecisionKind::Confirmed,
            evidence: "ok".into(),
            severity: Some("high".into()),
        });
    checkpoint
        .class_coverage
        .push(security_engineer::VulnerabilityClassCoverage {
            class_id: "auth_authorization".into(),
            class_name: "Authentication and authorization".into(),
            considered: true,
            applicable: true,
            hunted: true,
            ..Default::default()
        });
    for stage in [
        security_engineer::SecurityHarnessStage::Recon,
        security_engineer::SecurityHarnessStage::Hunt,
        security_engineer::SecurityHarnessStage::Validate,
        security_engineer::SecurityHarnessStage::Gapfill,
        security_engineer::SecurityHarnessStage::Dedupe,
        security_engineer::SecurityHarnessStage::Trace,
        security_engineer::SecurityHarnessStage::Feedback,
    ] {
        checkpoint
            .stage_history
            .push(security_engineer::StageHistoryEntry {
                stage,
                status: "completed".into(),
                started_at: 1,
                finished_at: 2,
                summary: "done".into(),
            });
    }

    let body = serde_json::to_string_pretty(&checkpoint).unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(
            crate::workspace::InMemoryWorkspace::new()
                .with_file("kb/security-harness/checkpoints/e2e-repair.json", &body),
        ) as Box<dyn crate::workspace::Workspace>,
    ));
    let llm = MockLlm::new(vec![
        // First report attempt: malformed JSON.
        mock_text_response("This is not JSON, I forgot."),
        // Repair attempt: valid report.
        mock_text_response(e2e_full_report_json()),
    ]);
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"resume": true, "run_id": "e2e-repair"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "repair path should succeed, got: {}",
        result.content
    );

    let guard = workspace.read().await;
    let body = guard
        .get("kb/security-harness/checkpoints/e2e-repair.json")
        .unwrap();
    let checkpoint: security_engineer::SecurityCheckpoint = serde_json::from_str(&body).unwrap();
    assert_eq!(
        checkpoint.report_validation_state.status, "valid",
        "after a successful repair, report_validation_state.status should be 'valid', got {:?}",
        checkpoint.report_validation_state.status
    );
    let report = checkpoint
        .report_draft
        .as_ref()
        .expect("report_draft should be set");
    assert!(
        report.findings.iter().any(|f| f.id == "finding-001"),
        "repaired report should contain finding-001"
    );
}

#[tokio::test]
async fn security_engineer_e2e_resume_at_validate() {
    // Pre-seed a checkpoint with Recon and Hunt already recorded as
    // completed and one finding in findings_so_far. Resume should pick
    // up at Validate. Recon + Hunt must NOT re-run.
    let dir = tempfile::tempdir().unwrap();
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "e2e-resume-validate".into(),
        security_engineer::TargetRef {
            repo_path: dir.path().display().to_string(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint.current_stage = security_engineer::SecurityHarnessStage::Validate;
    checkpoint
        .findings_so_far
        .push(security_engineer::SecurityFinding {
            id: "finding-001".into(),
            title: "missing owner predicate".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "HTTP request boundary".into(),
            entry_point: "src/proxy.rs:42".into(),
            sink_or_decision: "store.fetch_by_id".into(),
            root_cause: "owner predicate missing".into(),
            affected_paths: vec!["src/proxy.rs:42".into()],
            evidence: vec!["read_file evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "cross-tenant access".into(),
            severity_rationale: "high because reachable".into(),
            fix_recommendation: "add owner predicate".into(),
        });
    checkpoint
        .class_coverage
        .push(security_engineer::VulnerabilityClassCoverage {
            class_id: "auth_authorization".into(),
            class_name: "Authentication and authorization".into(),
            considered: true,
            applicable: true,
            hunted: true,
            ..Default::default()
        });
    for stage in [
        security_engineer::SecurityHarnessStage::Recon,
        security_engineer::SecurityHarnessStage::Hunt,
    ] {
        checkpoint
            .stage_history
            .push(security_engineer::StageHistoryEntry {
                stage,
                status: "completed".into(),
                started_at: 1,
                finished_at: 2,
                summary: "done".into(),
            });
    }

    let body = serde_json::to_string_pretty(&checkpoint).unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new().with_file(
            "kb/security-harness/checkpoints/e2e-resume-validate.json",
            &body,
        )) as Box<dyn crate::workspace::Workspace>,
    ));
    // A fresh MockLlm with NO fallback keeps the test honest — if resume
    // erroneously re-runs recon or hunt, those stages will fan out
    // dozens of specialists and exhaust the explicit queue, failing the
    // test loudly. We only enumerate validate + trace + report.
    let llm = MockLlm::new(vec![
        e2e_validate_confirm_response(),
        e2e_trace_reachable_response(),
        mock_text_response(e2e_full_report_json()),
    ]);
    let models_seen = llm.models_seen_handle();
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"resume": true, "run_id": "e2e-resume-validate"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "resume at validate should succeed, got: {}",
        result.content
    );

    let seen = models_seen.lock().unwrap().clone();
    // Three calls total: one each for validate, trace, report. Hunt
    // would have added ~24 (one per taxonomy class).
    assert_eq!(
        seen.len(),
        3,
        "resume at validate should invoke the LLM exactly 3 times (validate, trace, report), got {} calls: {:?}",
        seen.len(),
        seen
    );
}

#[tokio::test]
async fn security_engineer_e2e_resume_at_report() {
    // Pre-seed a checkpoint with Recon..Feedback already in stage_history
    // and the full evidence chain in place. Resume should only run the
    // report stage.
    let dir = tempfile::tempdir().unwrap();
    let mut checkpoint = security_engineer::SecurityCheckpoint::new(
        "e2e-resume-report".into(),
        security_engineer::TargetRef {
            repo_path: dir.path().display().to_string(),
            git_ref: None,
        },
        "scope".into(),
        security_engineer::ModelMetadata {
            provider: "test".into(),
            model: "test-model".into(),
        },
        1,
    );
    checkpoint.current_stage = security_engineer::SecurityHarnessStage::Report;
    checkpoint
        .findings_so_far
        .push(security_engineer::SecurityFinding {
            id: "finding-001".into(),
            title: "missing owner predicate".into(),
            severity: "high".into(),
            vulnerability_class: "auth_authorization".into(),
            trust_boundary: "HTTP request boundary".into(),
            entry_point: "src/proxy.rs:42".into(),
            sink_or_decision: "store.fetch_by_id".into(),
            root_cause: "owner predicate missing".into(),
            affected_paths: vec!["src/proxy.rs:42".into()],
            evidence: vec!["read_file evidence".into()],
            reachability: "reachable".into(),
            tenant_or_instance_impact: "cross-tenant access".into(),
            severity_rationale: "high because reachable".into(),
            fix_recommendation: "add owner predicate".into(),
        });
    checkpoint
        .validation_decisions_so_far
        .push(security_engineer::ValidationDecision {
            finding_id: "finding-001".into(),
            decision: security_engineer::ValidationDecisionKind::Confirmed,
            evidence: "ok".into(),
            severity: Some("high".into()),
        });
    checkpoint
        .trace_results_so_far
        .push(security_engineer::TraceResult {
            finding_id: "finding-001".into(),
            reachable: true,
            severity_effect: "keeps".into(),
            evidence: vec!["trace ev".into()],
        });
    checkpoint
        .class_coverage
        .push(security_engineer::VulnerabilityClassCoverage {
            class_id: "auth_authorization".into(),
            class_name: "Authentication and authorization".into(),
            considered: true,
            applicable: true,
            hunted: true,
            ..Default::default()
        });
    for stage in [
        security_engineer::SecurityHarnessStage::Recon,
        security_engineer::SecurityHarnessStage::Hunt,
        security_engineer::SecurityHarnessStage::Validate,
        security_engineer::SecurityHarnessStage::Gapfill,
        security_engineer::SecurityHarnessStage::Dedupe,
        security_engineer::SecurityHarnessStage::Trace,
        security_engineer::SecurityHarnessStage::Feedback,
    ] {
        checkpoint
            .stage_history
            .push(security_engineer::StageHistoryEntry {
                stage,
                status: "completed".into(),
                started_at: 1,
                finished_at: 2,
                summary: "done".into(),
            });
    }

    let body = serde_json::to_string_pretty(&checkpoint).unwrap();
    let workspace: crate::workspace::WorkspaceHandle = Arc::new(tokio::sync::RwLock::new(
        Box::new(crate::workspace::InMemoryWorkspace::new().with_file(
            "kb/security-harness/checkpoints/e2e-resume-report.json",
            &body,
        )) as Box<dyn crate::workspace::Workspace>,
    ));
    // A fresh MockLlm with NO fallback: only the report LLM call may
    // fire. If recon, hunt, validate, or trace re-runs, the queue runs
    // out and the test panics — exactly what we want.
    let llm = MockLlm::new(vec![mock_text_response(e2e_full_report_json())]);
    let models_seen = llm.models_seen_handle();
    let tool = OrchestratorTool::new(
        security_engineer_config(),
        LlmProvider::Anthropic,
        "claude-opus-4-20250514".into(),
        crate::agent::rate_limiter::RateLimitedHandle::unlimited(Box::new(llm)),
        Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        )),
        Some(Arc::clone(&workspace)),
        &[],
        vec![],
    );
    let mut ctx = ToolContext::for_test(dir.path());
    ctx.workspace = Some(Arc::clone(&workspace));
    let result = tool
        .run(
            &serde_json::json!({"resume": true, "run_id": "e2e-resume-report"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !result.is_error,
        "resume at report should succeed, got: {}",
        result.content
    );

    let seen = models_seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "resume at report should invoke the LLM exactly once (the report stage), got {} calls: {:?}",
        seen.len(),
        seen
    );
}
