//! Durable boundaries around the existing executor. The checkpoint is the
//! authority for a suspended run; transcripts and SSE are projections of it.
use super::{
    Agent,
    protocol::{RunEventKind, RunId, RunStatus},
    stream_handler::ToolCall,
    token_budget::TokenBudget,
};
use crate::{
    chat_history::ChatHistory,
    controller::Output,
    error::{DysonError, Result},
    message::Message,
    tool::ToolOutput,
};
pub use dyson_harness::continuation::{HumanAction, HumanAnswer, RunCursor, RunState, Transition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const RECORD: &str = "continuation";

/// Text adapters for terminal/Telegram controllers, using the same validated
/// signal contract as HTTP. Answers are JSON objects matching the shown form.
pub fn resume_command(
    store: &dyn ChatHistory,
    chat: &str,
    input: &str,
) -> Result<Option<(RunId, Option<HumanAnswer>)>> {
    let input = input.trim();
    if input != "/resume" && input != "/decline" && !input.starts_with("/answer ") {
        return Ok(None);
    }
    let record = RunCheckpoint::load(store, chat)?.ok_or_else(|| invalid("no saved run"))?;
    let answer = if input == "/resume" {
        None
    } else {
        let RunState::WaitingForInput { request } = &record.cursor.state else {
            return Err(invalid("this run is not waiting for input"));
        };
        Some(HumanAnswer {
            request_id: request.id.clone(),
            action: if input == "/decline" {
                HumanAction::Decline
            } else {
                HumanAction::Accept
            },
            content: if input == "/decline" {
                serde_json::Value::Null
            } else {
                serde_json::from_str(input.strip_prefix("/answer ").unwrap())?
            },
        })
    };
    record.validate_resume(&record.cursor.run_id, answer.as_ref())?;
    Ok(Some((record.cursor.run_id, answer)))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RunCheckpoint {
    version: u32,
    #[serde(default)]
    pub(super) direct: bool,
    pub cursor: RunCursor,
    #[serde(default)]
    accepted_answer: Option<HumanAnswer>,
    messages: Vec<Message>,
    turn_count: usize,
    budget: TokenBudget,
    model: String,
    max_iterations: usize,
    system_prompt_hash: String,
    tool_schema_hash: String,
    working_dir: std::path::PathBuf,
    counts: HashMap<String, usize>,
    admitted: HashSet<String>,
    failures: HashMap<String, usize>,
    observations: HashMap<String, (String, usize)>,
    pub(super) progress: super::r#loop::TurnProgress,
    warnings: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct DispatchReceipt {
    call: ToolCall,
    result: Option<SavedResult>,
}
#[derive(Serialize, Deserialize)]
struct SavedResult {
    output: ToolOutput,
    duration_ms: u64,
    error: Option<String>,
}

fn invalid(message: impl Into<String>) -> DysonError {
    DysonError::Llm(message.into())
}

impl RunCheckpoint {
    pub fn pending_input(&self, chat: &str) -> Option<serde_json::Value> {
        let RunState::WaitingForInput { request } = &self.cursor.state else {
            return None;
        };
        if self.accepted_answer.is_some() {
            return None;
        }
        Some(serde_json::json!({
            "id":format!("durable-{}", super::task::digest(&format!("{}:{}:{}", chat, self.cursor.run_id.0, request.id))),
            "server":"Dyson", "message":request.question, "requestedSchema":request.schema,
            "conversation_id":chat,"run_id":self.cursor.run_id,"request_id":request.id
        }))
    }

    pub fn accept_answer(
        &mut self,
        store: &dyn ChatHistory,
        chat: &str,
        answer: &HumanAnswer,
    ) -> Result<()> {
        self.validate_resume(&self.cursor.run_id, Some(answer))?;
        self.accepted_answer = Some(answer.clone());
        store.save_harness_record(chat, RECORD, &serde_json::to_value(self)?)
    }
    pub fn cancel(store: &dyn ChatHistory, chat: &str) -> Result<()> {
        if let Some(mut record) = Self::load(store, chat)? {
            if matches!(record.cursor.state, RunState::Finished { .. }) {
                return Ok(());
            }
            record.cursor.state = RunState::Finished {
                status: RunStatus::Cancelled,
            };
            store.save_harness_record(chat, RECORD, &serde_json::to_value(&record)?)?;
            let sequence = store
                .load_run_events(chat)?
                .iter()
                .filter(|e| e.run_id == record.cursor.run_id)
                .map(|e| e.sequence)
                .max()
                .unwrap_or(0)
                + 1;
            store.append_run_event(
                chat,
                &super::protocol::RunEvent::new(
                    sequence,
                    record.cursor.run_id,
                    record.turn_count,
                    RunEventKind::RunFinished {
                        status: RunStatus::Cancelled,
                    },
                ),
            )?;
        }
        Ok(())
    }
    pub fn load(store: &dyn ChatHistory, chat: &str) -> Result<Option<Self>> {
        let Some(value) = store.load_harness_record(chat, RECORD)? else {
            return Ok(None);
        };
        let record: Self = serde_json::from_value(value)?;
        if record.version != 1 {
            return Err(invalid("unsupported run checkpoint version"));
        }
        Ok(Some(record))
    }
    pub fn validate_resume(&self, run: &RunId, answer: Option<&HumanAnswer>) -> Result<()> {
        if &self.cursor.run_id != run {
            return Err(invalid("run ID does not match the saved continuation"));
        }
        if let (Some(saved), Some(answer)) = (&self.accepted_answer, answer) {
            if saved != answer {
                return Err(invalid("a different answer was already accepted"));
            }
        }
        match (&self.cursor.state, answer.or(self.accepted_answer.as_ref())) {
            (RunState::Running | RunState::Paused, None) => Ok(()),
            (RunState::WaitingForInput { request }, Some(answer))
                if request.id == answer.request_id =>
            {
                if answer.action == HumanAction::Accept {
                    crate::tool::validate_tool_input(&request.schema, &answer.content)
                        .map_err(invalid)?;
                }
                Ok(())
            }
            (RunState::WaitingForInput { .. }, _) => {
                Err(invalid("a matching human answer is required"))
            }
            (RunState::Finished { .. }, _) => Err(invalid("this run has already finished")),
            _ => Err(invalid("this run is not waiting for that answer")),
        }
    }
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({"run_id":self.cursor.run_id,"state":self.cursor.state,"answer_received":self.accepted_answer.is_some(),
            "iteration":self.cursor.iteration,"pending_tools":self.cursor.pending.iter().map(|c| serde_json::json!({"id":c.id,"name":c.name})).collect::<Vec<_>>()})
    }
}

impl Agent {
    pub(super) fn ensure_startable(&self) -> Result<()> {
        if let Some(backend) = &self.history_backend {
            if let Some(record) = RunCheckpoint::load(backend.store.as_ref(), &backend.chat_id)? {
                if !matches!(record.cursor.state, RunState::Finished { .. }) {
                    return Err(invalid(format!(
                        "Run {} is unfinished; resume or cancel it before starting another turn",
                        record.cursor.run_id.0
                    )));
                }
            }
        }
        Ok(())
    }
    fn schema_hash(&self) -> String {
        let mut definitions: Vec<_> = self
            .tool_registry
            .tools
            .iter()
            .map(|(name, tool)| (name, tool.input_schema()))
            .collect();
        definitions.sort_by(|a, b| a.0.cmp(b.0));
        super::task::digest(&serde_json::to_string(&definitions).expect("JSON tool schemas"))
    }

    pub(super) fn begin_continuation(&mut self, direct: bool) -> Result<()> {
        self.continuation = Some(RunCheckpoint {
            version: 1,
            direct,
            cursor: RunCursor::new(self.active_run_id.clone()),
            accepted_answer: None,
            messages: vec![],
            turn_count: self.conversation.turn_count,
            budget: self.conversation.token_budget.clone(),
            model: self.config.model.clone(),
            max_iterations: self.max_iterations,
            system_prompt_hash: super::task::digest(&self.system_prompt),
            tool_schema_hash: self.schema_hash(),
            working_dir: self.tool_context.working_dir.clone(),
            counts: HashMap::new(),
            admitted: HashSet::new(),
            failures: HashMap::new(),
            observations: HashMap::new(),
            progress: Default::default(),
            warnings: vec![],
        });
        self.checkpoint_run()
    }

    pub(super) fn checkpoint_run(&mut self) -> Result<()> {
        let Some(record) = &mut self.continuation else {
            return Ok(());
        };
        record.messages.clone_from(&self.conversation.messages);
        record.turn_count = self.conversation.turn_count;
        record.budget = self.conversation.token_budget.clone();
        record.counts = self.limiter.counts();
        record.failures.clone_from(&self.repeated_failures);
        record.observations.clone_from(&self.repeated_observations);
        record.warnings = self
            .run_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(backend) = &self.history_backend {
            backend.store.save_harness_record(
                &backend.chat_id,
                RECORD,
                &serde_json::to_value(record)?,
            )?;
        }
        Ok(())
    }

    pub(super) fn transition(&mut self, event: Transition) -> Result<()> {
        if let Some(record) = &mut self.continuation {
            if matches!(event, Transition::Wait(_)) {
                record.accepted_answer = None;
            }
            record.cursor = record.cursor.reduce(event).map_err(invalid)?;
        }
        self.checkpoint_run()
    }

    /// Cooperative pause at a persisted model/tool boundary. Does not cancel effects.
    pub fn request_pause(&self) -> Result<()> {
        let backend = self
            .history_backend
            .as_ref()
            .ok_or_else(|| invalid("durable history is required to pause"))?;
        backend.store.save_harness_record(
            &backend.chat_id,
            "pause-request",
            &serde_json::json!({"run_id":self.active_run_id}),
        )
    }

    pub(super) fn pause_at_boundary(&mut self) -> Result<bool> {
        if matches!(
            self.continuation.as_ref().map(|r| &r.cursor.state),
            Some(RunState::WaitingForInput { .. } | RunState::Paused)
        ) {
            return Ok(true);
        }
        let Some(backend) = &self.history_backend else {
            return Ok(false);
        };
        let pause = backend
            .store
            .load_harness_record(&backend.chat_id, "pause-request")?
            .is_some_and(|v| v["run_id"].as_str() == Some(self.active_run_id.0.as_str()));
        if pause {
            self.last_run_status = RunStatus::Paused;
            self.transition(Transition::Pause)?;
        }
        Ok(pause)
    }

    pub(super) fn tool_admitted(&mut self, id: &str) -> bool {
        self.continuation
            .as_mut()
            .is_some_and(|r| !r.admitted.insert(id.into()))
    }

    fn receipt_key(&self, call: &ToolCall) -> String {
        format!(
            "dispatch-{}",
            super::task::digest(&format!("{}:{}", self.active_run_id.0, call.id))
        )
    }

    /// Return a saved observation, never silently repeat a started side effect.
    pub(super) fn saved_dispatch(
        &self,
        call: &ToolCall,
    ) -> Result<Option<(ToolOutput, std::time::Duration)>> {
        let Some(backend) = &self.history_backend else {
            return Ok(None);
        };
        let Some(value) = backend
            .store
            .load_harness_record(&backend.chat_id, &self.receipt_key(call))?
        else {
            return Ok(None);
        };
        let receipt: DispatchReceipt = serde_json::from_value(value)?;
        if receipt.call != *call {
            return Err(invalid("saved invocation does not match pending tool"));
        }
        if let Some(result) = receipt.result {
            return Ok(Some((
                result.output,
                std::time::Duration::from_millis(result.duration_ms),
            )));
        }
        let events = backend.store.load_run_events(&backend.chat_id)?;
        let events: Vec<_> = events
            .iter()
            .filter(|e| e.run_id == self.active_run_id)
            .collect();
        let resolution = events.iter().find_map(|e| match &e.kind {
            RunEventKind::ToolReconciled {
                tool_use_id,
                resolution,
            } if tool_use_id == &call.id => Some(resolution),
            _ => None,
        });
        let finished = events.iter().any(|e| matches!(&e.kind, RunEventKind::ToolFinished { tool_use_id, .. } if tool_use_id == &call.id));
        if finished || resolution.is_some() {
            return Ok(Some((ToolOutput::error(format!("Recovered completed invocation; it was not repeated. {}", resolution.map_or("The result delivery was interrupted; inspect task evidence before continuing.", String::as_str))), std::time::Duration::ZERO)));
        }
        if events.iter().any(|e| matches!(&e.kind, RunEventKind::ToolStarted { tool_use_id, .. } if tool_use_id == &call.id)) {
            return Err(invalid(format!("tool {} has an unknown outcome; reconcile it before resuming", call.id)));
        }
        // The executor always syncs ToolStarted before dispatch. No such event
        // means the process stopped before the tool could run.
        Ok(None)
    }

    pub(super) fn save_dispatch(
        &self,
        call: &ToolCall,
        result: Option<&Result<(ToolOutput, std::time::Duration)>>,
    ) -> Result<()> {
        let Some(backend) = &self.history_backend else {
            return Ok(());
        };
        let result = result.map(|r| match r {
            Ok((out, duration)) => serde_json::json!({"output":out,"duration_ms":duration.as_millis() as u64,"error":null}),
            Err(error) => serde_json::json!({"output":ToolOutput::error(error.to_string()),"duration_ms":0,"error":error.to_string()}),
        });
        backend.store.save_harness_record(
            &backend.chat_id,
            &self.receipt_key(call),
            &serde_json::json!({"call":call,"result":result}),
        )
    }

    /// Continue the saved run without adding a user message or allocating a run ID.
    pub async fn resume_detailed(
        &mut self,
        run: &RunId,
        answer: Option<HumanAnswer>,
        output: &mut dyn Output,
    ) -> Result<super::protocol::RunOutcome> {
        let backend = self
            .history_backend
            .as_ref()
            .ok_or_else(|| invalid("durable history is required to resume"))?;
        let record = RunCheckpoint::load(backend.store.as_ref(), &backend.chat_id)?
            .ok_or_else(|| invalid("no saved run"))?;
        record.validate_resume(run, answer.as_ref())?;
        if record.direct && record.cursor.pending.len() != 1 {
            return Err(invalid(
                "direct command has no saved invocation; cancel this run",
            ));
        }
        let answer = answer.or_else(|| record.accepted_answer.clone());
        if record.model != self.config.model
            || record.system_prompt_hash != super::task::digest(&self.system_prompt)
            || record.tool_schema_hash != self.schema_hash()
            || record.working_dir != self.tool_context.working_dir
        {
            return Err(invalid(
                "model, prompt, tools or working directory changed; restore the run configuration before resuming",
            ));
        }
        let events = backend.store.load_run_events(&backend.chat_id)?;
        self.active_run_id = run.clone();
        *self
            .event_sequence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = events
            .iter()
            .filter(|e| &e.run_id == run)
            .map(|e| e.sequence)
            .max()
            .unwrap_or(0);
        self.tool_context.harness.ensure_loaded()?;
        for call in &record.cursor.pending {
            self.saved_dispatch(call)?;
        }
        self.conversation.messages = record.messages.clone();
        self.conversation.turn_count = record.turn_count;
        let configured_limit = self.conversation.token_budget.max_output_tokens;
        self.conversation.token_budget = record.budget.clone();
        if let Some(limit) = configured_limit {
            self.conversation.token_budget.max_output_tokens =
                Some(limit.min(record.budget.max_output_tokens.unwrap_or(limit)));
        }
        self.max_iterations = self.max_iterations.min(record.max_iterations);
        self.limiter.restore_counts(record.counts.clone());
        self.repeated_failures = record.failures.clone();
        self.repeated_observations = record.observations.clone();
        *self.run_warnings.lock().unwrap_or_else(|e| e.into_inner()) = record.warnings.clone();
        backend.store.save_harness_record(
            &backend.chat_id,
            "pause-request",
            &serde_json::json!({}),
        )?;
        self.continuation = Some(record);
        self.last_run_status = RunStatus::Partial;
        if let Some(answer) = answer {
            let declined = answer.action != HumanAction::Accept;
            self.conversation.messages.push(Message::tool_result(
                &answer.request_id,
                &serde_json::to_string(&answer)?,
                answer.action != HumanAction::Accept,
            ));
            self.transition(Transition::Answer(answer.request_id))?;
            if declined {
                let pending = self
                    .continuation
                    .as_ref()
                    .map(|r| r.cursor.pending.clone())
                    .unwrap_or_default();
                for call in pending {
                    self.conversation.messages.push(Message::tool_result(&call.id, "Not executed: the user declined or cancelled the preceding request. Reconsider the plan using their response.", true));
                    self.transition(Transition::Observed(call.id))?;
                }
            }
        } else {
            self.transition(Transition::Resume)?;
        }
        self.try_emit_run_event(RunEventKind::RunResumed)?;
        let before = self.conversation.token_budget.clone();
        if self.continuation.as_ref().is_some_and(|r| r.direct) {
            let call = self
                .continuation
                .as_ref()
                .and_then(|r| r.cursor.pending.first())
                .cloned()
                .ok_or_else(|| {
                    invalid("direct command has no saved invocation; cancel this run")
                })?;
            let result = self.execute_tool_call_durable(&call).await;
            if result.as_ref().is_ok_and(|(out, _)| !out.is_error) {
                self.last_run_status = RunStatus::Completed;
            }
            self.finish_run_protocol(&result);
            let text = result.map(|(out, _)| out.content).unwrap_or_default();
            output.text_delta(&text)?;
            output.flush()?;
            return Ok(self.detailed_outcome(&before, text));
        }
        self.resuming = true;
        let result = self.run_inner(output).await;
        self.resuming = false;
        self.finish_run_protocol(&result);
        Ok(self.detailed_outcome(&before, result.unwrap_or_default()))
    }
}
