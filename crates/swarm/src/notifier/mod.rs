//! Notifier — fires off notifications when tasks reach a terminal state.
//!
//! The notifier consumes "task done" events from the scheduler and
//! delivers each task's configured [`crate::scheduler::NotifyChannel`]s.
//! Three transports today:
//!
//! - [`Stdout`] — prints to the hub's tracing log; useful for local dev
//! - [`Webhook`] — POSTs JSON to a URL
//! - [`Telegram`] — POSTs to the Telegram bot API
//!
//! Each delivery is retried with exponential backoff up to a deadline
//! (default 1 hour). On final failure, the task row is left with
//! `notification_delivered = 0` so the next time an agent calls
//! `swarm_results` it can mention "notification was never delivered".
//!
//! Templates are `{{var}}` substitution only — no Handlebars, no Jinja.
//! Supported variables: `task_id`, `state`, `skill`, `summary`, `error`,
//! `duration_secs`, `assigned_node`.

mod template;

pub use template::render_template;

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::scheduler::{NotifyChannel, TaskRow, TaskStore};

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("template: {0}")]
    Template(String),
    #[error("retry deadline exceeded")]
    DeadlineExceeded,
}

/// Compact view of a finished task that the notifier renders into a
/// notification body / template.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalEvent {
    pub task_id: String,
    pub state: String,
    pub skill: Option<String>,
    pub summary: String,
    pub error: Option<String>,
    pub duration_secs: Option<u64>,
    pub assigned_node: Option<String>,
}

impl TerminalEvent {
    pub fn from_row(row: &TaskRow) -> Self {
        let summary = row
            .result_text
            .clone()
            .or_else(|| row.error.clone())
            .or_else(|| Some("(no output)".to_string()))
            .unwrap();
        Self {
            task_id: row.task_id.clone(),
            state: row.state.as_str().to_string(),
            skill: row.skill.clone(),
            summary,
            error: row.error.clone(),
            duration_secs: row.duration_secs,
            assigned_node: row.assigned_node.clone(),
        }
    }
}

/// Worker that consumes terminal events and dispatches notifications.
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::Sender<String>,
}

impl Notifier {
    /// Spawn a notifier worker. Returns the [`Notifier`] handle the
    /// scheduler uses to enqueue tasks for notification delivery.
    pub fn spawn(store: TaskStore) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            while let Some(task_id) = rx.recv().await {
                let store = store.clone();
                tokio::spawn(async move {
                    deliver_for_task(&store, &task_id).await;
                });
            }
        });
        Self { tx }
    }

    /// Enqueue a task for notification. Best-effort: if the channel is
    /// full or closed, the caller's `swarm_results` flag will still pick
    /// up "notification not delivered" later.
    pub async fn notify(&self, task_id: String) {
        if self.tx.send(task_id).await.is_err() {
            tracing::warn!("notifier channel closed");
        }
    }
}

async fn deliver_for_task(store: &TaskStore, task_id: &str) {
    let row = match store.get(task_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(%task_id, "notifier: task not found");
            return;
        }
        Err(e) => {
            tracing::error!(%task_id, error = %e, "notifier: failed to load task");
            return;
        }
    };

    if !row.is_terminal() {
        tracing::debug!(%task_id, state = row.state.as_str(), "notifier: task not terminal yet");
        return;
    }

    let channels: Vec<NotifyChannel> = match serde_json::from_str(&row.notify_json) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%task_id, error = %e, "notifier: invalid notify_json");
            return;
        }
    };

    if channels.is_empty() {
        // Nothing to do, but mark delivered so the flag is consistent.
        let _ = store.mark_notification_delivered(task_id).await;
        return;
    }

    let event = TerminalEvent::from_row(&row);
    let mut all_ok = true;
    for ch in channels {
        if let Err(e) = deliver_with_retry(&ch, &event).await {
            tracing::warn!(%task_id, error = %e, "notification delivery failed");
            all_ok = false;
        }
    }
    if all_ok {
        let _ = store.mark_notification_delivered(task_id).await;
    }
}

/// Backoff schedule (best-effort). Up to ~1 hour total.
const BACKOFFS: &[Duration] = &[
    Duration::from_secs(0),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(600),
    Duration::from_secs(1800),
];

async fn deliver_with_retry(
    channel: &NotifyChannel,
    event: &TerminalEvent,
) -> Result<(), NotifyError> {
    let mut last_err: Option<NotifyError> = None;
    for delay in BACKOFFS {
        if !delay.is_zero() {
            tokio::time::sleep(*delay).await;
        }
        match deliver_once(channel, event).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(error = %e, ?delay, "notification attempt failed");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or(NotifyError::DeadlineExceeded))
}

async fn deliver_once(channel: &NotifyChannel, event: &TerminalEvent) -> Result<(), NotifyError> {
    match channel {
        NotifyChannel::Stdout { template } => {
            let line = render_for_event(template.as_deref(), event)?;
            tracing::info!(target: "notifier", "{line}");
            Ok(())
        }
        NotifyChannel::Webhook { url, template } => {
            let body = if let Some(tpl) = template {
                serde_json::json!({
                    "text": render_for_event(Some(tpl), event)?,
                    "task": event,
                })
            } else {
                serde_json::json!({"task": event})
            };
            let resp = reqwest::Client::new()
                .post(url)
                .json(&body)
                .send()
                .await
                .map_err(|e| NotifyError::Transport(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(NotifyError::Transport(format!("HTTP {}", resp.status())));
            }
            Ok(())
        }
        NotifyChannel::Telegram {
            bot_token,
            chat_id,
            template,
        } => {
            let text = render_for_event(template.as_deref(), event)?;
            let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
            let resp = reqwest::Client::new()
                .post(&url)
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "text": text,
                }))
                .send()
                .await
                .map_err(|e| NotifyError::Transport(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(NotifyError::Transport(format!("HTTP {}", resp.status())));
            }
            Ok(())
        }
    }
}

fn render_for_event(
    template: Option<&str>,
    event: &TerminalEvent,
) -> Result<String, NotifyError> {
    let tpl = template.unwrap_or(DEFAULT_TEMPLATE);
    template::render_template(tpl, event).map_err(|e| NotifyError::Template(e.to_string()))
}

const DEFAULT_TEMPLATE: &str = "swarm task {{task_id}} {{state}} ({{skill}}): {{summary}}";

/// Convenience: spawn the notifier and the scheduler stall sweeper as
/// the hub starts up.
pub fn spawn_notifier(store: TaskStore) -> Arc<Notifier> {
    Arc::new(Notifier::spawn(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::TerminalStatus;

    #[tokio::test]
    async fn stdout_delivery_marks_flag() {
        let store = TaskStore::open_memory().unwrap();
        let id = store
            .submit(
                Some("bash".into()),
                "p".into(),
                vec![],
                None,
                serde_json::json!({}),
                vec![NotifyChannel::Stdout { template: None }],
            )
            .await
            .unwrap();
        store.mark_assigned(&id, "n").await.unwrap();
        store
            .finish(
                &id,
                TerminalStatus::Completed,
                Some("yay".into()),
                None,
                3,
            )
            .await
            .unwrap();

        let n = Notifier::spawn(store.clone());
        n.notify(id.clone()).await;
        // Give the spawned task a beat.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.notification_delivered);
    }

    #[tokio::test]
    async fn empty_channels_marks_delivered() {
        let store = TaskStore::open_memory().unwrap();
        let id = store
            .submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![])
            .await
            .unwrap();
        store.mark_assigned(&id, "n").await.unwrap();
        store
            .finish(&id, TerminalStatus::Completed, Some("ok".into()), None, 1)
            .await
            .unwrap();

        deliver_for_task(&store, &id).await;
        let row = store.get(&id).await.unwrap().unwrap();
        assert!(row.notification_delivered);
    }
}
