//! Stable, serializable run protocol shared by persistence, replay, evals,
//! controllers, and observability exporters.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::Idempotency;

pub const RUN_EVENT_SCHEMA_VERSION: u16 = 1;

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{:032x}", rand::random::<u128>())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    pub fn new() -> Self {
        Self(new_id("run"))
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Cancelled,
    BudgetExceeded,
    IterationLimit,
    Partial,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub llm_calls: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub status: RunStatus,
    pub final_text: String,
    pub usage: RunUsage,
    pub warnings: Vec<String>,
}

/// A side effect that started but has no durable terminal event. Recovery
/// code must surface these calls for operator/model reconciliation and must
/// never silently retry them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedToolOutcome {
    pub run_id: RunId,
    pub tool_use_id: String,
    pub effective_tool_name: String,
    pub idempotency_key: String,
}

/// Deterministic grading result for a persisted run trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvaluation {
    pub run_id: RunId,
    pub passed: bool,
    pub status: Option<RunStatus>,
    pub llm_attempts: usize,
    pub tool_calls_started: usize,
    pub tool_calls_failed: usize,
    pub unresolved_tools: Vec<UnresolvedToolOutcome>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEventKind {
    RunStarted,
    LlmAttemptStarted {
        iteration: usize,
        attempt: usize,
    },
    LlmAttemptCompleted {
        iteration: usize,
        output_tokens: usize,
        tool_calls: usize,
    },
    LlmAttemptFailed {
        iteration: usize,
        error_kind: String,
        retryable: bool,
        after_tool_use: bool,
    },
    ToolRequested {
        tool_use_id: String,
        tool_name: String,
        input_sha256: String,
    },
    ToolAuthorized {
        tool_use_id: String,
        effective_tool_name: String,
        idempotency: Idempotency,
        timeout_ms: u64,
    },
    ToolStarted {
        tool_use_id: String,
        effective_tool_name: String,
        idempotency_key: String,
    },
    ToolFinished {
        tool_use_id: String,
        effective_tool_name: String,
        is_error: bool,
        duration_ms: u64,
    },
    ToolOutcomeUnknown {
        tool_use_id: String,
        effective_tool_name: String,
        reason: String,
    },
    ContextCompacted {
        old_messages: usize,
        new_messages: usize,
    },
    RunFinished {
        status: RunStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub schema_version: u16,
    pub sequence: u64,
    pub unix_ms: u128,
    pub run_id: RunId,
    pub turn: usize,
    #[serde(flatten)]
    pub kind: RunEventKind,
}

impl RunEvent {
    pub fn new(sequence: u64, run_id: RunId, turn: usize, kind: RunEventKind) -> Self {
        Self {
            schema_version: RUN_EVENT_SCHEMA_VERSION,
            sequence,
            unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            run_id,
            turn,
            kind,
        }
    }
}

/// Reconstruct tools whose side effects may have happened but whose outcome
/// was never durably observed. This is the key crash-recovery invariant: an
/// unknown outcome is visible and is not converted into an automatic retry.
pub fn unresolved_tool_outcomes(events: &[RunEvent]) -> Vec<UnresolvedToolOutcome> {
    let mut active: HashMap<(RunId, String), UnresolvedToolOutcome> = HashMap::new();
    for event in events {
        match &event.kind {
            RunEventKind::ToolStarted {
                tool_use_id,
                effective_tool_name,
                idempotency_key,
            } => {
                active.insert(
                    (event.run_id.clone(), tool_use_id.clone()),
                    UnresolvedToolOutcome {
                        run_id: event.run_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        effective_tool_name: effective_tool_name.clone(),
                        idempotency_key: idempotency_key.clone(),
                    },
                );
            }
            RunEventKind::ToolFinished { tool_use_id, .. }
            | RunEventKind::ToolOutcomeUnknown { tool_use_id, .. } => {
                active.remove(&(event.run_id.clone(), tool_use_id.clone()));
            }
            _ => {}
        }
    }
    let mut unresolved: Vec<_> = active.into_values().collect();
    unresolved.sort_by(|a, b| {
        a.run_id
            .0
            .cmp(&b.run_id.0)
            .then_with(|| a.tool_use_id.cmp(&b.tool_use_id))
    });
    unresolved
}

/// Grade one run from its canonical event stream. The grader is deliberately
/// model-independent so the same assertions can be used for mocked CI runs,
/// live model matrices, and production replay.
pub fn evaluate_run(events: &[RunEvent], run_id: &RunId) -> RunEvaluation {
    let run_events: Vec<_> = events
        .iter()
        .filter(|event| &event.run_id == run_id)
        .collect();
    let mut failures = Vec::new();
    let starts = run_events
        .iter()
        .filter(|event| matches!(event.kind, RunEventKind::RunStarted))
        .count();
    if starts != 1 {
        failures.push(format!("expected one run_started event, found {starts}"));
    }

    let mut previous_sequence = None;
    let mut started_tools = HashSet::new();
    let mut terminal_tools = HashSet::new();
    let mut tool_calls_failed = 0;
    let mut status = None;
    let mut terminal_count = 0;
    let mut llm_attempts = 0;
    for event in &run_events {
        if previous_sequence.is_some_and(|previous| event.sequence <= previous) {
            failures.push("event sequence is not strictly increasing".to_string());
        }
        previous_sequence = Some(event.sequence);
        match &event.kind {
            RunEventKind::LlmAttemptStarted { .. } => llm_attempts += 1,
            RunEventKind::ToolStarted { tool_use_id, .. } => {
                match started_tools.insert(tool_use_id.clone()) {
                    true => {}
                    false => {
                        failures.push(format!("tool {tool_use_id} started more than once"));
                    }
                }
            }
            RunEventKind::ToolFinished {
                tool_use_id,
                is_error,
                ..
            } => {
                if !started_tools.contains(tool_use_id) {
                    failures.push(format!("tool {tool_use_id} finished without starting"));
                }
                if !terminal_tools.insert(tool_use_id.clone()) {
                    failures.push(format!("tool {tool_use_id} has multiple terminal events"));
                }
                tool_calls_failed += usize::from(*is_error);
            }
            RunEventKind::ToolOutcomeUnknown { tool_use_id, .. } => {
                if !started_tools.contains(tool_use_id) {
                    failures.push(format!(
                        "tool {tool_use_id} became unknown without starting"
                    ));
                }
                if !terminal_tools.insert(tool_use_id.clone()) {
                    failures.push(format!("tool {tool_use_id} has multiple terminal events"));
                }
                failures.push(format!(
                    "tool {tool_use_id} has an unknown side-effect outcome"
                ));
            }
            RunEventKind::RunFinished {
                status: terminal_status,
            } => {
                terminal_count += 1;
                status = Some(*terminal_status);
            }
            _ => {}
        }
    }
    if terminal_count != 1 {
        failures.push(format!(
            "expected one run_finished event, found {terminal_count}"
        ));
    }
    if !matches!(status, Some(RunStatus::Completed)) {
        failures.push(format!("run did not complete successfully: {status:?}"));
    }

    let unresolved_tools: Vec<_> = unresolved_tool_outcomes(events)
        .into_iter()
        .filter(|tool| &tool.run_id == run_id)
        .collect();
    if !unresolved_tools.is_empty() {
        failures.push(format!(
            "{} tool outcome(s) remain unresolved",
            unresolved_tools.len()
        ));
    }

    RunEvaluation {
        run_id: run_id.clone(),
        passed: failures.is_empty(),
        status,
        llm_attempts,
        tool_calls_started: started_tools.len(),
        tool_calls_failed,
        unresolved_tools,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_with_schema_version() {
        let event = RunEvent::new(7, RunId::new(), 3, RunEventKind::RunStarted);
        let encoded = serde_json::to_string(&event).unwrap();
        let decoded: RunEvent = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
        assert_eq!(decoded.schema_version, RUN_EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn run_ids_are_unique() {
        assert_ne!(RunId::new(), RunId::new());
    }

    #[test]
    fn replay_surfaces_in_flight_side_effects_and_grader_fails_closed() {
        let run_id = RunId::new();
        let events = vec![
            RunEvent::new(1, run_id.clone(), 1, RunEventKind::RunStarted),
            RunEvent::new(
                2,
                run_id.clone(),
                1,
                RunEventKind::ToolStarted {
                    tool_use_id: "call-1".into(),
                    effective_tool_name: "write_file".into(),
                    idempotency_key: "call-1".into(),
                },
            ),
        ];
        let evaluation = evaluate_run(&events, &run_id);
        assert!(!evaluation.passed);
        assert_eq!(evaluation.unresolved_tools.len(), 1);
        assert!(
            evaluation
                .failures
                .iter()
                .any(|f| f.contains("run_finished"))
        );
    }

    #[test]
    fn completed_trajectory_passes_deterministic_grading() {
        let run_id = RunId::new();
        let events = vec![
            RunEvent::new(1, run_id.clone(), 1, RunEventKind::RunStarted),
            RunEvent::new(
                2,
                run_id.clone(),
                1,
                RunEventKind::LlmAttemptStarted {
                    iteration: 0,
                    attempt: 0,
                },
            ),
            RunEvent::new(
                3,
                run_id.clone(),
                1,
                RunEventKind::RunFinished {
                    status: RunStatus::Completed,
                },
            ),
        ];
        let evaluation = evaluate_run(&events, &run_id);
        assert!(evaluation.passed, "{:?}", evaluation.failures);
        assert_eq!(evaluation.llm_attempts, 1);
    }
}
