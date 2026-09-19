//! Serializable control state. Transitions are pure; the host persists them
//! before performing effects and supplies observations on completion.
use crate::{RunId, RunStatus, ToolCall};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanInputRequest {
    pub id: String,
    pub question: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanAnswer {
    pub request_id: String,
    pub action: HumanAction,
    #[serde(default)]
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Running,
    Paused,
    WaitingForInput { request: HumanInputRequest },
    Finished { status: RunStatus },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunCursor {
    pub run_id: RunId,
    pub state: RunState,
    /// Next model iteration. A selected tool batch belongs to iteration - 1.
    pub iteration: usize,
    pub pending: Vec<ToolCall>,
    #[serde(default)]
    seen_tools: std::collections::BTreeSet<String>,
}

pub enum Transition {
    Selected(Vec<ToolCall>),
    Observed(String),
    Pause,
    Wait(HumanInputRequest),
    Resume,
    Answer(String),
    Finish(RunStatus),
}

impl RunCursor {
    pub fn new(run_id: RunId) -> Self {
        Self {
            run_id,
            state: RunState::Running,
            iteration: 0,
            pending: vec![],
            seen_tools: Default::default(),
        }
    }

    /// Reject invalid transitions without modifying the input state.
    pub fn reduce(&self, event: Transition) -> Result<Self, &'static str> {
        let mut next = self.clone();
        match event {
            Transition::Selected(calls)
                if self.state == RunState::Running && self.pending.is_empty() =>
            {
                let mut ids = std::collections::HashSet::new();
                if calls.iter().any(|c| {
                    c.id.is_empty() || self.seen_tools.contains(&c.id) || !ids.insert(&c.id)
                }) {
                    return Err("tool call IDs must be nonempty and unique");
                }
                next.seen_tools.extend(calls.iter().map(|c| c.id.clone()));
                next.pending = calls;
                next.iteration += 1;
            }
            Transition::Observed(id) if self.pending.iter().any(|c| c.id == id) => {
                next.pending.retain(|c| c.id != id);
            }
            Transition::Pause if self.state == RunState::Running => next.state = RunState::Paused,
            Transition::Wait(request)
                if self.state == RunState::Running
                    && self.pending.iter().any(|c| c.id == request.id) =>
            {
                next.state = RunState::WaitingForInput { request };
            }
            Transition::Resume if matches!(self.state, RunState::Paused | RunState::Running) => {
                next.state = RunState::Running
            }
            Transition::Answer(id) if matches!(&self.state, RunState::WaitingForInput { request } if request.id == id) =>
            {
                next.pending.retain(|c| c.id != id);
                next.state = RunState::Running;
            }
            Transition::Finish(status) if !matches!(self.state, RunState::Finished { .. }) => {
                next.state = RunState::Finished { status }
            }
            _ => return Err("invalid run transition"),
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_and_human_answer_preserve_pending_work() {
        let initial = RunCursor::new(RunId("test".into()));
        let call = ToolCall::new("write", serde_json::json!({"path":"out"}));
        let selected = initial
            .reduce(Transition::Selected(vec![call.clone()]))
            .unwrap();
        let paused = selected.reduce(Transition::Pause).unwrap();
        let restored: RunCursor =
            serde_json::from_str(&serde_json::to_string(&paused).unwrap()).unwrap();
        assert_eq!(restored.reduce(Transition::Resume).unwrap(), selected);
        let waiting = selected
            .reduce(Transition::Wait(HumanInputRequest {
                id: call.id.clone(),
                question: "Proceed?".into(),
                schema: serde_json::json!({}),
            }))
            .unwrap();
        assert!(waiting.reduce(Transition::Answer("wrong".into())).is_err());
        assert!(waiting.reduce(Transition::Resume).is_err());
        let answered = waiting.reduce(Transition::Answer(call.id.clone())).unwrap();
        assert!(answered.pending.is_empty());
        assert!(answered.reduce(Transition::Answer(call.id)).is_err());
    }
}
