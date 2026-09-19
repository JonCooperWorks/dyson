use super::*;
use crate::agent::continuation::{HumanAction, HumanAnswer, RunCheckpoint, Transition};
use crate::chat_history::{ChatHistory, DiskChatHistory};

fn response(calls: &[ToolCall]) -> Vec<StreamEvent> {
    let mut events: Vec<_> = calls
        .iter()
        .map(|c| StreamEvent::ToolUseComplete {
            id: c.id.clone(),
            name: c.name.clone(),
            input: c.input.clone(),
        })
        .collect();
    events.push(StreamEvent::MessageComplete {
        stop_reason: StopReason::ToolUse,
        output_tokens: Some(9),
    });
    events
}
fn done() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("Finished".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            output_tokens: Some(3),
        },
    ]
}
fn store(dir: &Path) -> Arc<dyn ChatHistory> {
    Arc::new(DiskChatHistory::new_from_connection_string(dir.to_str().unwrap()).unwrap())
}

struct CountTool {
    count: Arc<AtomicUsize>,
    hang: bool,
}
#[async_trait::async_trait]
impl Tool for CountTool {
    fn name(&self) -> &str {
        "count_effect"
    }
    fn description(&self) -> &str {
        "A test effect"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn execution_plan(
        &self,
        _: &serde_json::Value,
        _: &ToolContext,
    ) -> crate::tool::ToolExecutionPlan {
        crate::tool::ToolExecutionPlan::write(format!("test:effect:{:p}", Arc::as_ptr(&self.count)))
    }
    async fn run(&self, _: &serde_json::Value, _: &ToolContext) -> Result<ToolOutput> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::success("Effect happened"))
    }
}
fn agent(
    store: Arc<dyn ChatHistory>,
    count: Arc<AtomicUsize>,
    responses: Vec<Vec<StreamEvent>>,
    hang: bool,
) -> Agent {
    let (mut agent, _) = make_agent_with_history(vec![], responses, CompactionConfig::default());
    agent
        .tool_registry
        .register_extra_tool(Arc::new(CountTool { count, hang }));
    agent.set_chat_history(store, "durable-test".into());
    agent
}
fn effect() -> ToolCall {
    ToolCall {
        id: "effect-1".into(),
        name: "count_effect".into(),
        input: serde_json::json!({}),
    }
}

struct PauseOutput {
    store: Arc<dyn ChatHistory>,
}
impl Output for PauseOutput {
    fn text_delta(&mut self, _: &str) -> Result<()> {
        self.tool_use_complete()
    }
    fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn tool_use_complete(&mut self) -> Result<()> {
        let record = RunCheckpoint::load(self.store.as_ref(), "durable-test")?.unwrap();
        self.store.save_harness_record(
            "durable-test",
            "pause-request",
            &serde_json::json!({"run_id":record.cursor.run_id}),
        )
    }
    fn tool_result(&mut self, _: &ToolOutput) -> Result<()> {
        Ok(())
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
async fn completed_response_wins_over_late_pause() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let mut runner = agent(
        storage.clone(),
        Arc::new(AtomicUsize::new(0)),
        vec![done()],
        false,
    );
    let outcome = runner
        .run_detailed(
            "finish",
            &mut PauseOutput {
                store: storage.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::Completed);
    assert!(matches!(
        RunCheckpoint::load(storage.as_ref(), "durable-test")
            .unwrap()
            .unwrap()
            .cursor
            .state,
        continuation::RunState::Finished {
            status: protocol::RunStatus::Completed
        }
    ));
}

#[tokio::test]
async fn direct_command_recovers_saved_result_without_model_or_reexecution() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = agent(storage.clone(), count.clone(), vec![], false);
    first.begin_run_protocol_with_mode(true).unwrap();
    let call = effect();
    first
        .transition(Transition::Selected(vec![call.clone()]))
        .unwrap();
    first.execute_tool_call_durable(&call).await.unwrap();
    let run = first.active_run_id.clone();
    drop(first); // Crash after saving the result, before finishing the run.
    let mut second = agent(store(dir.path()), count.clone(), vec![], false);
    let outcome = second
        .resume_detailed(&run, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(outcome.run_id, run);
    // The mutation happened, but no independent task verification ran.
    assert_eq!(outcome.status, protocol::RunStatus::Partial);
    assert_eq!(outcome.final_text, "Effect happened");
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pause_before_dispatch_restart_and_resume_same_run_once() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = agent(
        storage.clone(),
        count.clone(),
        vec![response(&[effect()])],
        false,
    );
    let outcome = first
        .run_detailed(
            "perform effect",
            &mut PauseOutput {
                store: storage.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::Paused);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let usage = first.token_budget().output_tokens_used;
    drop(first);
    let mut second = agent(store(dir.path()), count.clone(), vec![done()], false);
    let result = second
        .resume_detailed(&outcome.run_id, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(result.run_id, outcome.run_id);
    assert_eq!(result.final_text, "Finished");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(second.token_budget().output_tokens_used > usage);
    assert!(
        second
            .resume_detailed(&outcome.run_id, None, &mut SilentOutput)
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let events = storage.load_run_events("durable-test").unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.kind, protocol::RunEventKind::RunStarted))
            .count(),
        1
    );
}

#[tokio::test]
async fn human_question_survives_restart_validates_answer_and_blocks_later_tools() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let ask = ToolCall {
        id: "question-1".into(),
        name: "request_human_input".into(),
        input: serde_json::json!({"question":"Which branch?"}),
    };
    let mut first = agent(
        storage.clone(),
        count.clone(),
        vec![response(&[ask, effect()])],
        false,
    );
    let outcome = first.run_detailed("work", &mut SilentOutput).await.unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::WaitingForInput);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    drop(first);
    let mut second = agent(store(dir.path()), count.clone(), vec![done()], false);
    let mut record = RunCheckpoint::load(storage.as_ref(), "durable-test")
        .unwrap()
        .unwrap();
    assert!(record.pending_input("durable-test").is_some());
    let mut answer = HumanAnswer {
        request_id: "question-1".into(),
        action: HumanAction::Accept,
        content: serde_json::json!({"answer":""}),
    };
    assert!(
        second
            .resume_detailed(&outcome.run_id, Some(answer.clone()), &mut SilentOutput)
            .await
            .is_err()
    );
    assert!(
        second
            .run_detailed("overwrite", &mut SilentOutput)
            .await
            .is_err()
    );
    answer.content = serde_json::json!({"answer":"main"});
    record
        .accept_answer(storage.as_ref(), "durable-test", &answer)
        .unwrap();
    // Simulate a crash after the HTTP answer was acknowledged but before execution.
    let result = second
        .resume_detailed(&outcome.run_id, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(result.run_id, outcome.run_id);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(second.messages().iter().any(|m|m.content.iter().any(|c|matches!(c,ContentBlock::ToolResult{tool_use_id,content,..} if tool_use_id == "question-1" && content.contains("main")))));
    assert!(
        second
            .resume_detailed(&outcome.run_id, Some(answer), &mut SilentOutput)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn committed_tool_receipt_survives_crash_before_transcript_update() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = agent(storage.clone(), count.clone(), vec![], false);
    first.conversation.messages.push(Message::user("work"));
    first.begin_run_protocol().unwrap();
    let run = first.active_run_id.clone();
    let call = effect();
    first
        .conversation
        .messages
        .push(Message::assistant(vec![ContentBlock::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        }]));
    first
        .transition(Transition::Selected(vec![call.clone()]))
        .unwrap();
    first.execute_tool_call_durable(&call).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    drop(first);
    let mut second = agent(store(dir.path()), count.clone(), vec![done()], false);
    second
        .resume_detailed(&run, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(second.messages().iter().any(|m| m.content.iter().any(
        |c| matches!(c,ContentBlock::ToolResult{content,..} if content.contains("Effect happened"))
    )));
}

#[tokio::test]
async fn interrupted_side_effect_requires_reconciliation_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = agent(storage.clone(), count.clone(), vec![], false);
    first.conversation.messages.push(Message::user("work"));
    first.begin_run_protocol().unwrap();
    let call = effect();
    first
        .conversation
        .messages
        .push(Message::assistant(vec![ContentBlock::ToolUse {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        }]));
    first
        .transition(Transition::Selected(vec![call.clone()]))
        .unwrap();
    first.save_dispatch(&call, None).unwrap();
    first
        .try_emit_run_event(protocol::RunEventKind::ToolStarted {
            tool_use_id: call.id.clone(),
            effective_tool_name: call.name,
            idempotency_key: "external-operation".into(),
        })
        .unwrap();
    // Fault fixture: the external system committed, but the process died
    // before it could journal an observation. No process-global test lease.
    count.fetch_add(1, Ordering::SeqCst);
    let run = first.active_run_id.clone();
    drop(first);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let mut second = agent(store(dir.path()), count.clone(), vec![done()], false);
    assert!(
        second
            .resume_detailed(&run, None, &mut SilentOutput)
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    second
        .reconcile_tool_outcome(
            &run,
            "effect-1",
            "Effect independently confirmed; do not repeat",
        )
        .unwrap();
    second
        .resume_detailed(&run, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn declined_answer_withholds_the_remaining_selected_actions() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let ask = ToolCall {
        id: "q".into(),
        name: "request_human_input".into(),
        input: serde_json::json!({"question":"Proceed?"}),
    };
    let mut first = agent(
        storage.clone(),
        count.clone(),
        vec![response(&[ask, effect()])],
        false,
    );
    let waiting = first.run_detailed("work", &mut SilentOutput).await.unwrap();
    drop(first);
    let mut second = agent(storage, count.clone(), vec![done()], false);
    second
        .resume_detailed(
            &waiting.run_id,
            Some(HumanAnswer {
                request_id: "q".into(),
                action: HumanAction::Decline,
                content: serde_json::Value::Null,
            }),
            &mut SilentOutput,
        )
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resume_does_not_reset_iteration_limit_or_accept_changed_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let storage = store(dir.path());
    let count = Arc::new(AtomicUsize::new(0));
    let mut first = agent(
        storage.clone(),
        count.clone(),
        vec![response(&[effect()])],
        false,
    );
    first.max_iterations = 1;
    let paused = first
        .run_detailed(
            "work",
            &mut PauseOutput {
                store: storage.clone(),
            },
        )
        .await
        .unwrap();
    drop(first);
    let mut second = agent(storage, count.clone(), vec![done()], false);
    let model = second.config.model.clone();
    second.config.model = "changed-model".into();
    assert!(
        second
            .resume_detailed(&paused.run_id, None, &mut SilentOutput)
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    second.config.model = model;
    let outcome = second
        .resume_detailed(&paused.run_id, None, &mut SilentOutput)
        .await
        .unwrap();
    assert_eq!(outcome.status, protocol::RunStatus::IterationLimit);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
