//! Report each tool shell this runtime starts to Swarm, so the Security page
//! can name the tool call behind a process the VM's sensor saw.
//!
//! The sensor records the shell's exec (a `bash` whose parent is this
//! process) but nothing about the tool call; this records the call id, tool
//! name and the wall-clock time just before the spawn, on the same guest
//! clock. Swarm pairs the two. Only ids and names leave the VM, never the
//! command. Best effort: a report that cannot be delivered is dropped and
//! the process simply shows as an unlinked tool call.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const PATH: &str = "/v1/internal/security/tool-spawns";
const MAX_QUEUED: usize = 2000;
const FLUSH_EVERY: Duration = Duration::from_secs(2);
const MAX_BATCH: usize = 200;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Spawn {
    pub tool_use_id: String,
    pub tool_name: String,
    pub spawned_ms: i64,
}

fn queue() -> &'static Mutex<Vec<Spawn>> {
    static QUEUE: OnceLock<Mutex<Vec<Spawn>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Milliseconds since the epoch, taken immediately before `spawn()`.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Queue one report. No-op outside Swarm or without a tool call id.
pub fn record(tool_use_id: Option<&str>, tool_name: &str, spawned_ms: i64) {
    let Some(id) = tool_use_id.filter(|id| !id.is_empty() && id.len() <= 256) else {
        return;
    };
    if crate::swarm_cost::runtime_proxy_parts().is_none() {
        return;
    }
    let mut queue = queue()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.len() >= MAX_QUEUED {
        queue.remove(0);
    }
    queue.push(Spawn {
        tool_use_id: id.to_owned(),
        tool_name: tool_name.chars().take(128).collect(),
        spawned_ms,
    });
    drop(queue);
    ensure_flusher();
}

fn ensure_flusher() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.get().is_some() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.set(()).is_ok() {
        handle.spawn(async {
            loop {
                tokio::time::sleep(FLUSH_EVERY).await;
                flush().await;
            }
        });
    }
}

fn drain_batch(queue: &mut Vec<Spawn>) -> Vec<Spawn> {
    let n = queue.len().min(MAX_BATCH);
    queue.drain(..n).collect()
}

fn take_batch() -> Vec<Spawn> {
    drain_batch(
        &mut queue()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

async fn flush() {
    loop {
        let batch = take_batch();
        if batch.is_empty() {
            return;
        }
        let Some((base, token)) = crate::swarm_cost::runtime_proxy_parts() else {
            return;
        };
        let Some((apex, _)) = base.split_once("/llm") else {
            return;
        };
        let sent = crate::http::client()
            .post(format!("{apex}{PATH}"))
            .bearer_auth(&token)
            .timeout(Duration::from_secs(5))
            .json(&serde_json::json!({ "spawns": batch }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        if let Err(error) = sent {
            tracing::debug!(error = %error, "tool spawn report dropped");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_are_bounded_and_ordered() {
        let mut q: Vec<Spawn> = (0..250)
            .map(|i| Spawn {
                tool_use_id: format!("t{i}"),
                tool_name: "bash".into(),
                spawned_ms: i,
            })
            .collect();
        let first = drain_batch(&mut q);
        assert_eq!(first.len(), MAX_BATCH);
        assert_eq!(first[0].tool_use_id, "t0");
        assert_eq!(drain_batch(&mut q).len(), 50);
        assert!(drain_batch(&mut q).is_empty());
    }

    #[test]
    fn outside_swarm_nothing_is_queued() {
        if crate::swarm_cost::runtime_proxy_parts().is_some() {
            return;
        }
        let before = queue().lock().unwrap().len();
        record(Some("toolu_1"), "bash", 1);
        assert_eq!(queue().lock().unwrap().len(), before);
    }
}
