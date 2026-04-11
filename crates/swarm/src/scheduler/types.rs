//! Domain types for the scheduler.

use std::time::SystemTime;

use dyson_swarm_protocol::types::Payload;
use serde::{Deserialize, Serialize};

use crate::router::RoutingConstraints;

/// What an MCP caller submits when starting a long-running task.
#[derive(Debug, Clone, Deserialize)]
pub struct SubmitRequest {
    /// Free-form prompt for the agent.
    pub prompt: String,

    /// Skill / capability hint. Routed to a node that advertises this
    /// capability. Equivalent to `constraints.needs_capability`.
    #[serde(default)]
    pub skill: Option<String>,

    /// Inputs.
    #[serde(default)]
    pub payloads: Vec<Payload>,

    /// Hard timeout (seconds). `None` means no agent-level timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Routing constraints (gpu, capability, ram).
    #[serde(default)]
    pub constraints: Option<SubmittedConstraints>,

    /// Notification channels to fire when this task reaches a terminal
    /// state.
    #[serde(default)]
    pub notify: Vec<NotifyChannel>,
}

/// Constraints as they appear in the JSON request — converted to
/// [`RoutingConstraints`] internally.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubmittedConstraints {
    #[serde(default)]
    pub needs_gpu: bool,
    #[serde(default)]
    pub needs_capability: Option<String>,
    #[serde(default)]
    pub min_ram_gb: Option<u64>,
}

impl SubmittedConstraints {
    pub fn into_routing(self, default_capability: Option<String>) -> RoutingConstraints {
        RoutingConstraints {
            needs_gpu: self.needs_gpu,
            needs_capability: self.needs_capability.or(default_capability),
            min_ram_gb: self.min_ram_gb,
        }
    }
}

/// A notification channel a caller can attach to a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyChannel {
    /// Print a line to the hub's stdout / tracing log when the task
    /// finishes. Useful for local development.
    Stdout {
        /// Optional `{{var}}` template. Defaults to a built-in line.
        #[serde(default)]
        template: Option<String>,
    },

    /// POST a JSON body to a webhook URL.
    Webhook {
        url: String,
        #[serde(default)]
        template: Option<String>,
    },

    /// Send a Telegram message via the bot API.
    Telegram {
        bot_token: String,
        chat_id: String,
        #[serde(default)]
        template: Option<String>,
    },
}

/// The lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Submitted but not yet routed to a node.
    Pending,
    /// Assigned to a worker; not yet acknowledged or progressing.
    Assigned,
    /// Worker is actively running it (last progress heartbeat was
    /// recent).
    Running,
    /// Worker hasn't reported progress in too long. The task is still
    /// "alive" — the scheduler does not auto-reassign.
    Stalled,
    /// Cancellation has been requested; waiting for the worker to
    /// acknowledge or for the grace timer to expire.
    Cancelling,
    /// Final: completed successfully.
    Done,
    /// Final: failed.
    Failed,
    /// Final: cancelled (with or without a checkpoint).
    Cancelled,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Assigned => "assigned",
            Self::Running => "running",
            Self::Stalled => "stalled",
            Self::Cancelling => "cancelling",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "assigned" => Self::Assigned,
            "running" => Self::Running,
            "stalled" => Self::Stalled,
            "cancelling" => Self::Cancelling,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// A more specific terminal status, recorded once a task reaches a final
/// state. The scheduler stores this as JSON in the task row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalStatus {
    Completed,
    Failed { error: String },
    Cancelled { reason: Option<String> },
}

#[derive(Debug, Clone)]
pub struct TerminalState {
    pub status: TerminalStatus,
    pub finished_at: SystemTime,
    pub duration_secs: u64,
}

/// A row in the task table — full state of one task.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task_id: String,
    pub state: TaskState,
    pub skill: Option<String>,
    pub prompt: String,
    pub assigned_node: Option<String>,
    pub submitted_at: SystemTime,
    pub started_at: Option<SystemTime>,
    pub finished_at: Option<SystemTime>,
    pub last_progress_at: Option<SystemTime>,
    pub progress_pct: Option<f32>,
    pub progress_message: Option<String>,
    pub result_text: Option<String>,
    pub result_payloads_json: Option<String>,
    pub error: Option<String>,
    pub duration_secs: Option<u64>,
    pub notify_json: String,
    /// `notification_delivered=1` once the notifier acks delivery.
    pub notification_delivered: bool,
}

impl TaskRow {
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

/// A worker → hub progress heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressReport {
    pub task_id: String,
    /// 0.0 - 1.0 (or null if the skill can't estimate).
    #[serde(default)]
    pub progress: Option<f32>,
    /// Free-form human-readable status line.
    #[serde(default)]
    pub message: Option<String>,
    /// Optional log chunk to append.
    #[serde(default)]
    pub log: Option<String>,
}

/// Compact result row used by the `swarm_results` MCP tool.
#[derive(Debug, Clone, Serialize)]
pub struct RecentResult {
    pub task_id: String,
    pub state: String,
    pub skill: Option<String>,
    pub submitted_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub duration_secs: Option<u64>,
    pub assigned_node: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}
