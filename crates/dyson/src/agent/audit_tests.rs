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

#[tokio::test]
async fn task_completion_requires_observed_evidence() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![text("all done", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    let plan = agent.execute_tool_direct("task_control", serde_json::json!({"action":"plan","objective":"fix the test","criteria":[{"id":"tests","description":"test passes","tool":"bash","contains":"test result: ok"}]})).await.unwrap();
    assert!(
        !plan.is_error,
        "task contract must be accepted: {}",
        plan.content
    );
    let outcome = agent.run_detailed("continue", &mut output).await.unwrap();
    assert_eq!(
        outcome.status,
        protocol::RunStatus::Partial,
        "a claim of completion is not evidence"
    );
}

#[tokio::test]
async fn task_checkpoint_survives_agent_reconstruction() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().to_path_buf()).unwrap());
    let (mut agent, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    agent.set_chat_history(store.clone(), "durable".into());
    let result = agent.execute_tool_direct("task_control", serde_json::json!({"action":"plan","objective":"ship invoices","criteria":[{"id":"test","description":"passes","tool":"bash","contains":"ok"}]})).await.unwrap();
    assert!(!result.is_error);
    drop(agent);
    let (mut restored, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    restored.set_chat_history(store, "durable".into());
    let status = restored
        .execute_tool_direct("task_control", serde_json::json!({"action":"status"}))
        .await
        .unwrap();
    assert!(status.content.contains("ship invoices"));
}

struct SlowSharedMutation(Arc<AtomicUsize>, Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for SlowSharedMutation {
    fn name(&self) -> &str {
        "shared_mutation"
    }
    fn description(&self) -> &str {
        "claims the same resource across conversations"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn run(&self, _: &serde_json::Value, _: &crate::tool::ToolContext) -> Result<ToolOutput> {
        let active = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        self.1.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        self.0.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::success("done"))
    }
}
#[tokio::test]
async fn separate_conversations_serialize_shared_mutations() {
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let (mut b, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let max = Arc::new(AtomicUsize::new(0));
    let tool = Arc::new(SlowSharedMutation(
        Arc::new(AtomicUsize::new(0)),
        max.clone(),
    ));
    a.tool_registry.register_extra_tool(tool.clone());
    b.tool_registry.register_extra_tool(tool);
    let (x, y) = tokio::join!(
        a.execute_tool_direct("shared_mutation", serde_json::json!({})),
        b.execute_tool_direct("shared_mutation", serde_json::json!({}))
    );
    assert!(x.is_ok() && y.is_ok());
    assert_eq!(max.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn task_input_budget_blocks_before_dispatch() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![text("should never run", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    agent
        .tool_context
        .harness
        .budget
        .lock()
        .unwrap()
        .limits
        .max_input_tokens = Some(0);
    let result = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(result.usage.llm_calls, 0);
    assert_eq!(result.status, protocol::RunStatus::BudgetExceeded);
}
#[tokio::test]
async fn task_deadline_blocks_before_dispatch() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![text("should never run", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    agent
        .tool_context
        .harness
        .budget
        .lock()
        .unwrap()
        .limits
        .max_elapsed_ms = Some(0);
    let result = agent.run_detailed("work", &mut output).await.unwrap();
    assert_eq!(result.usage.llm_calls, 0);
    assert_eq!(result.status, protocol::RunStatus::BudgetExceeded);
}

#[tokio::test]
async fn another_conversation_invalidates_stale_verification() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proof");
    std::fs::write(&path, "PASS").unwrap();
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let (mut b, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    a.execute_tool_direct("task_control",serde_json::json!({"action":"plan","objective":"verify file","criteria":[{"id":"proof","description":"file passes","tool":"read_file","contains":"PASS"}]})).await.unwrap();
    let read = a
        .execute_tool_direct("read_file", serde_json::json!({"file_path":path}))
        .await
        .unwrap();
    let id = read.metadata.unwrap()["evidence_id"]
        .as_str()
        .unwrap()
        .to_string();
    b.execute_tool_direct(
        "write_file",
        serde_json::json!({"file_path":path,"content":"FAIL"}),
    )
    .await
    .unwrap();
    let result = a
        .execute_tool_direct(
            "task_control",
            serde_json::json!({"action":"verify","criterion":"proof","evidence_id":id}),
        )
        .await;
    assert!(
        result.is_err() || result.unwrap().is_error,
        "another conversation changed the verified resource"
    );
}

#[tokio::test]
async fn original_evidence_survives_compaction_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proof");
    std::fs::write(
        &path,
        format!(
            "{}MIDDLE_DECISIVE_EVIDENCE{}",
            "x".repeat(12000),
            "z".repeat(12000)
        ),
    )
    .unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().join("history")).unwrap());
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    a.set_chat_history(store.clone(), "evidence".into());
    let read = a
        .execute_tool_direct("read_file", serde_json::json!({"file_path":path}))
        .await
        .unwrap();
    let id = read.metadata.unwrap()["evidence_id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(a);
    let (mut b, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    b.set_chat_history(store, "evidence".into());
    let page = b
        .execute_tool_direct(
            "task_control",
            serde_json::json!({"action":"evidence","evidence_id":id,"offset":11000,"limit":3000}),
        )
        .await
        .unwrap();
    assert!(page.content.contains("MIDDLE_DECISIVE_EVIDENCE"));
}

struct KeyedReceiptTool(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for KeyedReceiptTool {
    fn name(&self) -> &str {
        "keyed_receipt"
    }
    fn description(&self) -> &str {
        "idempotent fixture"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn execution_plan(
        &self,
        _: &serde_json::Value,
        _: &crate::tool::ToolContext,
    ) -> crate::tool::ToolExecutionPlan {
        let mut p = crate::tool::ToolExecutionPlan::write("receipt:test");
        p.idempotency = crate::tool::Idempotency::Keyed;
        p
    }
    fn idempotency_key(
        &self,
        _: &serde_json::Value,
        _: &crate::tool::ToolContext,
    ) -> Option<String> {
        Some("operation-42".into())
    }
    async fn run(
        &self,
        _: &serde_json::Value,
        ctx: &crate::tool::ToolContext,
    ) -> Result<ToolOutput> {
        assert_eq!(ctx.idempotency_key.as_deref(), Some("operation-42"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::success("receipt-42"))
    }
    async fn lookup_result(
        &self,
        key: &str,
        _: &crate::tool::ToolContext,
    ) -> Result<Option<ToolOutput>> {
        assert_eq!(key, "operation-42");
        Ok(Some(ToolOutput::success("provider-receipt-42")))
    }
}
#[tokio::test]
async fn keyed_receipts_deduplicate_across_restart_and_reject_changed_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().join("history")).unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    a.set_chat_history(store.clone(), "receipts".into());
    a.tool_registry
        .register_extra_tool(Arc::new(KeyedReceiptTool(count.clone())));
    assert_eq!(
        a.execute_tool_direct("keyed_receipt", serde_json::json!({"amount":1}))
            .await
            .unwrap()
            .content,
        "receipt-42"
    );
    drop(a);
    let (mut b, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    b.set_chat_history(store, "receipts".into());
    b.tool_registry
        .register_extra_tool(Arc::new(KeyedReceiptTool(count.clone())));
    assert_eq!(
        b.execute_tool_direct("keyed_receipt", serde_json::json!({"amount":1}))
            .await
            .unwrap()
            .content,
        "receipt-42"
    );
    assert!(
        b.execute_tool_direct("keyed_receipt", serde_json::json!({"amount":2}))
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn keyed_unknown_uses_provider_lookup_without_reexecuting() {
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let count = Arc::new(AtomicUsize::new(0));
    a.tool_registry
        .register_extra_tool(Arc::new(KeyedReceiptTool(count.clone())));
    a.tool_context
        .harness
        .receipt_write(
            &super::super::task::digest("keyed_receipt:operation-42"),
            &super::super::task::digest("{}"),
            None,
        )
        .unwrap();
    assert_eq!(
        a.execute_tool_direct("keyed_receipt", serde_json::json!({}))
            .await
            .unwrap()
            .content,
        "provider-receipt-42"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
#[test]
fn child_budget_reservations_cannot_overspend_parent() {
    use super::super::task::{
        TaskRuntime,
        budget::{Price, Reservation},
    };
    let parent = TaskRuntime::default();
    {
        let mut b = parent.budget.lock().unwrap();
        b.limits.max_cost_microusd = Some(10);
        b.limits.prices.insert(
            "m".into(),
            Price {
                input_microusd_per_million: 1_000_000,
                output_microusd_per_million: 1_000_000,
            },
        );
    }
    let child = parent.child();
    let (hold, cap) = Reservation::reserve(parent.budget.clone(), "m", 2, 8).unwrap();
    assert_eq!(cap, 8);
    assert!(Reservation::reserve(child.budget.clone(), "m", 1, 1).is_err());
    hold.settle(Some(2), 3);
    assert_eq!(parent.budget.lock().unwrap().cost_microusd, 5);
    let (_second, cap) = Reservation::reserve(child.budget, "m", 1, 100).unwrap();
    assert_eq!(cap, 4);
}
#[test]
fn cost_budget_refuses_unknown_prices() {
    let runtime = super::super::task::TaskRuntime::default();
    runtime.budget.lock().unwrap().limits.max_cost_microusd = Some(10);
    assert!(
        super::super::task::budget::Reservation::reserve(runtime.budget, "unpriced", 1, 1).is_err()
    );
}

#[tokio::test]
async fn verified_completion_includes_real_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proof");
    std::fs::write(&path, "PASS").unwrap();
    let (mut a, mut output) = make_agent_with_history(
        vec![],
        vec![text("done", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    a.execute_tool_direct("task_control",serde_json::json!({"action":"plan","objective":"verify file","criteria":[{"id":"proof","description":"file passes","tool":"read_file","contains":"PASS"}]})).await.unwrap();
    let read = a
        .execute_tool_direct("read_file", serde_json::json!({"file_path":path}))
        .await
        .unwrap();
    let id = read.metadata.unwrap()["evidence_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !a.execute_tool_direct(
            "task_control",
            serde_json::json!({"action":"verify","criterion":"proof","evidence_id":id})
        )
        .await
        .unwrap()
        .is_error
    );
    let outcome = a.run_detailed("report", &mut output).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::Completed);
    assert_eq!(outcome.task.unwrap()["completion"], "verified");
}
struct ChangingFailure;
#[async_trait::async_trait]
impl Tool for ChangingFailure {
    fn name(&self) -> &str {
        "different_failure"
    }
    fn description(&self) -> &str {
        "different failed commands without progress"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn execution_plan(
        &self,
        _: &serde_json::Value,
        _: &crate::tool::ToolContext,
    ) -> crate::tool::ToolExecutionPlan {
        crate::tool::ToolExecutionPlan::read("failure-fixture")
    }
    async fn run(&self, _: &serde_json::Value, _: &crate::tool::ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::error("same underlying failure"))
    }
}
#[tokio::test]
async fn changing_commands_without_progress_pauses_task() {
    let streams = (0..20)
        .map(|i| {
            vec![
                StreamEvent::ToolUseComplete {
                    id: format!("f{i}"),
                    name: "different_failure".into(),
                    input: serde_json::json!({"attempt":i}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    output_tokens: Some(1),
                },
            ]
        })
        .collect();
    let (mut a, mut output) = make_agent_with_history(vec![], streams, CompactionConfig::default());
    a.tool_registry
        .register_extra_tool(Arc::new(ChangingFailure));
    let outcome = a.run_detailed("fix this", &mut output).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::Partial);
    assert_eq!(outcome.usage.llm_calls, 16);
    assert!(outcome.final_text.contains("Task paused"));
}
#[tokio::test]
async fn failed_provider_retries_consume_shared_budget() {
    use super::super::task::{
        TaskRuntime,
        budget::{ACTIVE, charge_uncertain_retry},
    };
    let runtime = TaskRuntime::default();
    runtime.budget.lock().unwrap().limits.max_output_tokens = Some(10);
    ACTIVE
        .scope(runtime.clone(), async {
            charge_uncertain_retry("m", 2, 6).unwrap();
            assert!(charge_uncertain_retry("m", 2, 6).is_err());
        })
        .await;
    assert_eq!(runtime.budget.lock().unwrap().output_tokens, 10);
}

#[tokio::test]
async fn stale_read_cannot_overwrite_another_conversations_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared");
    std::fs::write(&path, "original").unwrap();
    let (mut a, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    let (mut b, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    a.execute_tool_direct("read_file", serde_json::json!({"file_path":path}))
        .await
        .unwrap();
    b.execute_tool_direct(
        "write_file",
        serde_json::json!({"file_path":path,"content":"b change"}),
    )
    .await
    .unwrap();
    let result = a
        .execute_tool_direct(
            "write_file",
            serde_json::json!({"file_path":path,"content":"stale a change"}),
        )
        .await;
    assert!(result.is_err() || result.unwrap().is_error);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "b change");
}

#[tokio::test]
async fn recovered_foreign_mutation_blocks_conflicting_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared");
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().join("history")).unwrap());
    let (mut old, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    old.set_chat_history(store.clone(), "other".into());
    old.tool_context.harness.checkpoint().unwrap();
    let run = protocol::RunId::new();
    let operation = format!("{}:lost", run.0);
    store
        .append_run_event(
            "other",
            &protocol::RunEvent::new(
                1,
                run,
                1,
                protocol::RunEventKind::ToolStarted {
                    tool_use_id: "lost".into(),
                    effective_tool_name: "write_file".into(),
                    idempotency_key: "lost".into(),
                },
            ),
        )
        .unwrap();
    let mut checkpoint = store.load_harness_record("other", "task").unwrap().unwrap();
    checkpoint["pending"] = serde_json::json!({operation: [{"key":format!("file:{}",path.display()),"access":"write"}]});
    store
        .save_harness_record("other", "task", &checkpoint)
        .unwrap();
    drop(old);
    let (mut current, _) = make_agent_with_history(vec![], vec![], CompactionConfig::default());
    current.set_chat_history(store, "current".into());
    let result = current
        .execute_tool_direct(
            "write_file",
            serde_json::json!({"file_path":path,"content":"bad"}),
        )
        .await;
    assert!(result.is_err() || result.unwrap().is_error);
    assert!(!path.exists());
}

#[tokio::test]
async fn explicit_user_resumption_releases_no_progress_pause() {
    let (mut agent, mut output) = make_agent_with_history(
        vec![],
        vec![text("resumed", StopReason::EndTurn)],
        CompactionConfig::default(),
    );
    for _ in 0..16 {
        agent
            .tool_context
            .harness
            .record(
                "failed",
                &serde_json::json!({}),
                &ToolOutput::error("same failure"),
                &crate::tool::ToolExecutionPlan::read("task:test"),
            )
            .unwrap();
    }
    let outcome = agent
        .run_detailed("try this new approach", &mut output)
        .await
        .unwrap();
    assert_eq!(outcome.final_text, "resumed");
}

#[test]
fn restored_task_respects_tighter_operator_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(crate::chat_history::DiskChatHistory::new(dir.path().to_path_buf()).unwrap());
    let original = super::task::TaskRuntime::default();
    original.attach(store.clone(), "limits".into());
    original.budget.lock().unwrap().limits.max_input_tokens = Some(1000);
    original.checkpoint().unwrap();
    let restored = super::task::TaskRuntime::default();
    restored.budget.lock().unwrap().limits.max_input_tokens = Some(100);
    restored.attach(store, "limits".into());
    assert_eq!(
        restored.budget.lock().unwrap().limits.max_input_tokens,
        Some(100)
    );
}

#[test]
fn task_contract_schema_teaches_the_model_required_criterion_fields() {
    let schema = super::task::TaskTool.input_schema();
    let criterion = &schema["properties"]["criteria"]["items"];
    for name in ["id", "description", "tool", "contains"] {
        assert_eq!(
            criterion["properties"][name]["type"], "string",
            "criterion field {name} must be discoverable"
        );
    }
}
