use super::*;
use crate::chat_history::ChatHistory;

fn text(text: &str, reason: StopReason) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::MessageComplete {
            stop_reason: reason,
            output_tokens: Some(7),
        },
    ]
}

#[tokio::test]
async fn truncation_exhaustion_is_not_completed() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![
            text("unfinished", StopReason::MaxTokens),
            text("partial summary", StopReason::EndTurn),
        ],
        CompactionConfig::default(),
    );
    agent.max_iterations = 1;
    let outcome = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::IterationLimit);
}

#[tokio::test]
async fn exhausted_budget_does_not_dispatch_another_model_call() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![text("should not run", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    agent.token_budget_mut().max_output_tokens = Some(0);
    let outcome = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(outcome.usage.llm_calls, 0);
    assert_eq!(outcome.status, protocol::RunStatus::BudgetExceeded);
}

#[tokio::test]
async fn failed_run_keeps_structured_outcome() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![vec![StreamEvent::Error(DysonError::Llm(
            "fatal fixture error".into(),
        ))]],
        CompactionConfig::default(),
    );
    let outcome = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::Failed);
    assert!(!outcome.warnings.is_empty());
}

#[tokio::test]
async fn compaction_preserves_tool_evidence_and_accounts_for_usage() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages = vec![
        Message::user("goal"),
        Message::tool_result(
            "t",
            "FAILED test_invoice: expected 42 got 0; artifact /tmp/evidence.json",
            true,
        ),
        Message::user("next"),
    ];
    let (mut agent, mut output) = make_agent_with_history(
        messages,
        vec![],
        CompactionConfig {
            protect_head: 1,
            protect_tail_tokens: 0,
            ..Default::default()
        },
    );
    agent.client = rate_limiter::RateLimitedHandle::unlimited(Box::new(RecordingMessagesLlm::new(
        vec![text(
            "Goal: fix invoice; evidence retained",
            StopReason::EndTurn,
        )],
        seen.clone(),
    )));
    agent.compact(&mut output).await.unwrap();
    let captured = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
    assert!(captured.contains("expected 42 got 0"));
    assert_eq!(agent.token_budget().output_tokens_used, 7);
    assert_eq!(agent.token_budget().llm_calls, 1);
}

struct BrokenJournal;
impl ChatHistory for BrokenJournal {
    fn save(&self, _: &str, _: &[Message]) -> Result<()> {
        Ok(())
    }
    fn load(&self, _: &str) -> Result<Vec<Message>> {
        Ok(vec![])
    }
    fn rotate(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn append_run_event(&self, _: &str, _: &protocol::RunEvent) -> Result<()> {
        Err(DysonError::Llm("disk full".into()))
    }
}

#[tokio::test]
async fn journal_failure_prevents_file_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("must-not-exist");
    let (mut agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    agent.set_chat_history(Arc::new(BrokenJournal), "audit".into());
    let _ = agent
        .execute_tool_direct(
            "write_file",
            serde_json::json!({"file_path":path,"content":"side effect"}),
        )
        .await;
    assert!(!path.exists());
}

#[tokio::test]
async fn unresolved_prior_mutation_blocks_new_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().join("history")).unwrap());
    let id = protocol::RunId::new();
    store
        .append_run_event(
            "audit",
            &protocol::RunEvent::new(
                1,
                id,
                1,
                protocol::RunEventKind::ToolStarted {
                    tool_use_id: "old".into(),
                    effective_tool_name: "write_file".into(),
                    idempotency_key: "old-key".into(),
                },
            ),
        )
        .unwrap();
    let path = dir.path().join("must-not-exist");
    let (mut agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    agent.set_chat_history(store, "audit".into());
    let _ = agent
        .execute_tool_direct(
            "write_file",
            serde_json::json!({"file_path":path,"content":"side effect"}),
        )
        .await;
    assert!(!path.exists());
}

#[tokio::test]
async fn tool_limit_spans_all_batches_in_a_user_turn() {
    let (mut agent, mut output) =
        make_agent_with_history(vec![], vec![], CompactionConfig::default());
    // Each batch below is handled by a separate model iteration.
    let calls = (0..51)
        .map(|i| {
            vec![
                StreamEvent::ToolUseComplete {
                    id: format!("c{i}"),
                    name: "missing".into(),
                    input: serde_json::json!({}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: Some(1),
                },
            ]
        })
        .chain(std::iter::once(text("done", StopReason::EndTurn)))
        .collect();
    agent.client = rate_limiter::RateLimitedHandle::unlimited(Box::new(MockLlm::new(calls)));
    agent.run("work", &mut output).await.unwrap();
    assert!(
        serde_json::to_string(agent.messages())
            .unwrap()
            .contains("per-turn limit exceeded")
    );
}

struct FailedDelivery;
impl Output for FailedDelivery {
    fn text_delta(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
    fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn tool_use_complete(&mut self) -> Result<()> {
        Ok(())
    }
    fn tool_result(&mut self, _: &ToolOutput) -> Result<()> {
        Err(DysonError::Llm("disconnected".into()))
    }
    fn send_file(&mut self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn error(&mut self, _: &DysonError) -> Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn delivery_failure_preserves_all_executed_results() {
    let (mut agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let calls = vec![
        stream_handler::ToolCall::new("missing_a", serde_json::json!({})),
        stream_handler::ToolCall::new("missing_b", serde_json::json!({})),
    ];
    let _ = agent.execute_tool_calls(&calls, &mut FailedDelivery).await;
    for call in calls {
        assert!(agent.messages().iter().any(|m| m.content.iter().any(
            |b| matches!(b,ContentBlock::ToolResult{tool_use_id,..} if tool_use_id==&call.id)
        )));
    }
}

struct CountMutation(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for CountMutation {
    fn name(&self) -> &str {
        "count_mutation"
    }
    fn description(&self) -> &str {
        "test mutation"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}})
    }
    async fn run(&self, _: &serde_json::Value, _: &crate::tool::ToolContext) -> Result<ToolOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::success("mutated"))
    }
}
#[tokio::test]
async fn reflection_validates_before_mutating() {
    let calls = vec![vec![
        StreamEvent::ToolUseComplete {
            id: "c".into(),
            name: "count_mutation".into(),
            input: serde_json::json!({}),
        },
        StreamEvent::MessageComplete {
            stop_reason: StopReason::ToolUse,
            output_tokens: Some(1),
        },
    ]];
    let (agent, _) = make_agent_with_history(vec![], calls, CompactionConfig::default());
    let ctx = dream::DreamContext {
        client: agent.client.clone(),
        config: agent.config.clone(),
        sandbox: agent.sandbox.clone(),
        history: None,
        tool_context: agent.tool_context.clone(),
        conversation_summary: "test".into(),
        turn_count: 1,
    };
    let count = Arc::new(AtomicUsize::new(0));
    reflection::run_mini_loop(
        &ctx,
        "test",
        vec![Arc::new(CountMutation(count.clone()))],
        "test",
        1,
        "test",
    )
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn file_execution_plans_use_the_actual_schema_field() {
    let (agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    for name in ["read_file", "write_file", "edit_file"] {
        let tool = agent.tool_registry.get(name).unwrap();
        let plan = tool.execution_plan(
            &serde_json::json!({"file_path":"invoice.rs"}),
            &agent.tool_context,
        );
        assert!(
            plan.resources
                .iter()
                .any(|r| r.key.ends_with("/invoice.rs")),
            "{name}: {plan:?}"
        );
    }
}

#[tokio::test]
async fn repeated_failures_trigger_a_change_of_approach() {
    let mut responses = Vec::new();
    for i in 0..4 {
        responses.push(vec![
            StreamEvent::ToolUseComplete {
                id: format!("repeat-{i}"),
                name: "missing".into(),
                input: serde_json::json!({"same":true}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: Some(1),
            },
        ]);
    }
    responses.push(text("blocked", StopReason::EndTurn));
    let (mut agent, mut output) =
        make_agent_with_history(vec![], responses, CompactionConfig::default());
    agent.run("work", &mut output).await.unwrap();
    assert!(
        serde_json::to_string(agent.messages())
            .unwrap()
            .contains("NO PROGRESS")
    );
}

#[tokio::test]
async fn explicit_reconciliation_releases_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().join("history")).unwrap());
    let id = protocol::RunId::new();
    store
        .append_run_event(
            "audit",
            &protocol::RunEvent::new(
                1,
                id.clone(),
                1,
                protocol::RunEventKind::ToolStarted {
                    tool_use_id: "old".into(),
                    effective_tool_name: "write_file".into(),
                    idempotency_key: "old-key".into(),
                },
            ),
        )
        .unwrap();
    let (mut agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    agent.set_chat_history(store, "audit".into());
    agent
        .reconcile_tool_outcome(
            &id,
            "old",
            "Operator inspected destination; original mutation did not occur",
        )
        .unwrap();
    let path = dir.path().join("recovered");
    let result = agent
        .execute_tool_direct(
            "write_file",
            serde_json::json!({"file_path":path,"content":"recovered"}),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "recovered");
}

#[tokio::test]
async fn failed_stream_accounts_for_visible_generation() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![vec![
            StreamEvent::TextDelta("The partial response consumed tokens before failure".into()),
            StreamEvent::Error(DysonError::Llm("fatal".into())),
        ]],
        CompactionConfig::default(),
    );
    let outcome = agent.run_detailed("work", &mut output).await.unwrap();
    assert!(outcome.usage.output_tokens > 0);
    assert_eq!(outcome.usage.llm_calls, 1);
}

#[tokio::test]
async fn budget_reserves_a_final_tool_free_summary() {
    let responses = vec![
        vec![
            StreamEvent::ToolUseComplete {
                id: "last-work".into(),
                name: "missing".into(),
                input: serde_json::json!({}),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                output_tokens: Some(90),
            },
        ],
        text("Remaining work documented", StopReason::EndTurn),
    ];
    let (mut agent, mut output) =
        make_agent_with_history(vec![], responses, CompactionConfig::default());
    agent.token_budget_mut().max_output_tokens = Some(100);
    let outcome = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::BudgetExceeded);
    assert!(
        serde_json::to_string(agent.messages())
            .unwrap()
            .contains("Do NOT call any tools")
    );
    assert_eq!(outcome.usage.output_tokens, 97);
}

#[tokio::test]
async fn repeated_unchanged_reads_surface_lack_of_progress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unchanged");
    std::fs::write(&path, "no new information").unwrap();
    let (mut agent, mut output) =
        make_agent_with_history(vec![], vec![], CompactionConfig::default());
    for _ in 0..5 {
        let call =
            stream_handler::ToolCall::new("read_file", serde_json::json!({"file_path":path}));
        agent
            .execute_tool_calls(&[call], &mut output)
            .await
            .unwrap();
    }
    assert!(
        serde_json::to_string(agent.messages())
            .unwrap()
            .contains("NO PROGRESS")
    );
}
