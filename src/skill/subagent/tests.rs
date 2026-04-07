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
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        crate::auth::Credential::new(String::new()),
        None,
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
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        crate::auth::Credential::new(String::new()),
        None,
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

/// Helper to create a SubagentTool that uses a MockLlm.
///
/// Since SubagentTool creates its own LlmClient internally via
/// `crate::llm::create_client()`, we can't inject a MockLlm directly.
/// Instead, we test the full flow by constructing a child Agent
/// manually and running it — this tests the same code path that
/// SubagentTool::run() uses.
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
        crate::agent::Agent::new(Box::new(llm), sandbox, skills, &settings, None, 0).unwrap();
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
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        crate::auth::Credential::new(String::new()),
        None,
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
    };

    let tool = SubagentTool::new(
        config,
        LlmProvider::Anthropic,
        crate::auth::Credential::new(String::new()),
        None,
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
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

    assert_eq!(skill.name(), "subagents");
    assert_eq!(skill.tools().len(), 1);
    assert_eq!(skill.tools()[0].name(), "research_agent");

    let prompt = skill.system_prompt().unwrap();
    assert!(prompt.contains("research_agent"));
    assert!(prompt.contains("Research specialist"));
    assert!(prompt.contains("subagents"));
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
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

    // Should have skipped the subagent with unknown provider.
    assert_eq!(skill.tools().len(), 0);
    assert!(skill.system_prompt().is_none());
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
fn builtin_subagent_configs_returns_planner_and_researcher() {
    let configs = builtin_subagent_configs();
    assert_eq!(configs.len(), 2);
    assert_eq!(configs[0].name, "planner");
    assert_eq!(configs[1].name, "researcher");
    // Both use the "default" provider.
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
    }];

    let sandbox: Arc<dyn Sandbox> = Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox);
    let skill = SubagentSkill::new(&configs, &settings, sandbox, None, &[]);

    // Should have resolved successfully (1 tool, not skipped).
    assert_eq!(skill.tools().len(), 1);
    assert_eq!(skill.tools()[0].name(), "test_default");
}
