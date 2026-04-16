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
}

impl MockLlm {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
        }
    }
}

#[async_trait]
impl crate::llm::LlmClient for MockLlm {
    async fn stream(
        &self,
        _messages: &[crate::message::Message],
        _system: &str,
        _system_suffix: &str,
        _tools: &[crate::llm::ToolDefinition],
        _config: &crate::llm::CompletionConfig,
    ) -> Result<crate::llm::StreamResponse> {
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
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry);

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
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry);

    // Should have skipped the subagent with unknown provider,
    // but the built-in coder and security_engineer are always present.
    assert_eq!(skill.tools().len(), 2);
    assert_eq!(skill.tools()[0].name(), "coder");
    assert_eq!(skill.tools()[1].name(), "security_engineer");
    assert!(skill.system_prompt().unwrap().contains("coder"));
    assert!(skill.system_prompt().unwrap().contains("security_engineer"));
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
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry);

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
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry);

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
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[], &registry);

    // Should have resolved successfully (1 config-driven + 1 coder + 1 security_engineer = 3 tools).
    assert_eq!(skill.tools().len(), 3);
    assert_eq!(skill.tools()[0].name(), "test_default");
    assert_eq!(skill.tools()[1].name(), "coder");
    assert_eq!(skill.tools()[2].name(), "security_engineer");
}

// -----------------------------------------------------------------------
// CoderTool tests
// -----------------------------------------------------------------------

/// Helper to create a CoderTool with no inherited tools for metadata tests.
fn make_coder_tool() -> CoderTool {
    CoderTool::new(
        LlmProvider::Anthropic,
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
        post_validator: None,
        max_validator_retries: 0,
    };
    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
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
    assert_eq!(config.max_iterations, 40);
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
        post_validator: None,
        max_validator_retries: 0,
    };

    let parent_tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(crate::tool::bash::BashTool::default()),
        Arc::new(crate::tool::read_file::ReadFileTool),
        Arc::new(crate::tool::write_file::WriteFileTool),
    ];

    let tool = OrchestratorTool::new(
        config,
        LlmProvider::Anthropic,
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
