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

/// Correlate a launched process tree with the active tool/agent observation.
/// Set only on this Command; never mutate the process-wide environment.
/// Descendants inherit the execution ID unless they explicitly clear it.
pub fn correlate(cmd: &mut tokio::process::Command, tool_use_id: Option<&str>) -> String {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let execution = format!("{:032x}", rand::random::<u128>());
    // Clear inherited IDs first: an absent current span must not misattribute
    // this invocation to whichever process launched Dyson.
    for key in [
        "DYSON_EXECUTION_ID",
        "DYSON_TRACE_ID",
        "DYSON_SPAN_ID",
        "DYSON_TOOL_CALL_ID",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("DYSON_EXECUTION_ID", &execution);
    let span = tracing::Span::current();
    span.set_attribute("dyson.execution.id", execution.clone());
    let context = span.context();
    let borrowed = context.span();
    let identity = borrowed.span_context();
    if identity.is_valid() {
        cmd.env("DYSON_TRACE_ID", identity.trace_id().to_string());
        cmd.env("DYSON_SPAN_ID", identity.span_id().to_string());
    }
    if let Some(id) = tool_use_id.filter(|id| {
        !id.is_empty()
            && id.len() <= 256
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    }) {
        cmd.env("DYSON_TOOL_CALL_ID", id);
    }
    execution
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
    #[tokio::test]
    async fn execution_ids_are_per_invocation_and_inherited_by_descendants() {
        let mut first = tokio::process::Command::new("sh");
        first
            .env_clear()
            .env("DYSON_TRACE_ID", "stale")
            .env("DYSON_TOOL_CALL_ID", "stale");
        first.arg("-c").arg("printf '%s|%s|%s|' \"$DYSON_EXECUTION_ID\" \"$DYSON_TRACE_ID\" \"$DYSON_TOOL_CALL_ID\"; sh -c 'printf %s \"$DYSON_EXECUTION_ID\"'");
        let id = correlate(&mut first, Some("call-1"));
        let output = first.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{id}||call-1|{id}")
        );
        let mut second = tokio::process::Command::new("sh");
        assert_ne!(id, correlate(&mut second, None));
    }

    #[test]
    fn execution_id_is_attached_to_the_active_trace_span() {
        use opentelemetry::trace::TracerProvider;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::prelude::*;
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        let execution = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("tool.execute");
            let _entered = span.enter();
            let mut command = tokio::process::Command::new("sh");
            let id = correlate(&mut command, Some("call-1"));
            let env: std::collections::HashMap<_, _> = command
                .as_std()
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|s| s.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            assert_eq!(env["DYSON_TRACE_ID"].as_ref().unwrap().len(), 32);
            assert_eq!(env["DYSON_SPAN_ID"].as_ref().unwrap().len(), 16);
            id
        });
        let spans = memory.get_finished_spans().unwrap();
        assert!(spans[0].attributes.iter().any(|a| a.key.as_str() == "dyson.execution.id" && a.value.to_string() == execution));
    }
}
