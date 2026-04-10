// ===========================================================================
// Swarm controller — receive and execute tasks from a swarm hub.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `Controller` trait for swarm participation.  When
//   configured, this controller connects to a swarm hub, registers the
//   node's hardware manifest, and enters a loop: receive signed tasks
//   via SSE, verify signatures, fetch payloads, run the agent, and
//   send results back.
//
// How it fits:
//
//   dyson.json:
//     { "type": "swarm", "url": "...", "public_key": "v1:..." }
//
//   listen.rs sees "swarm" controller:
//     1. Auto-injects hub as MCP skill → all agents get swarm_dispatch
//     2. Creates this SwarmController → this node receives tasks
//
//   SwarmController::run():
//     1. Build agent (shared ClientRegistry)
//     2. Probe hardware + read agent's tool names → NodeManifest
//     3. POST /swarm/register → get node_id + auth token
//     4. GET /swarm/events → open SSE stream
//     5. Spawn heartbeat background task
//     6. Loop: SSE event → verify → fetch blobs → agent.run() → POST result
//     7. Deregister on shutdown
//
// Social contract:
//   Adding this controller means "I can use the swarm, and the swarm
//   can use me."  The public key ensures only the configured hub can
//   send tasks.
// ===========================================================================

use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::config::{Settings, SwarmControllerConfig};
use crate::controller::ClientRegistry;
use crate::error::DysonError;
use crate::skill::subagent::CaptureOutput;
use crate::swarm::connection::{SwarmConnection, SwarmEvent};
use crate::swarm::probe::HardwareProbe;
use crate::swarm::types::{
    NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus,
};
use crate::swarm::verify::{SwarmPublicKey, verify_signed_payload};

/// Delay between heartbeats.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// SwarmController
// ---------------------------------------------------------------------------

/// Controller that connects to a swarm hub and executes tasks.
pub struct SwarmController {
    config: SwarmControllerConfig,
    public_key: SwarmPublicKey,
}

impl SwarmController {
    /// Create a new SwarmController from config.
    ///
    /// Returns `None` if the public key is invalid.
    pub fn from_config(
        config: &crate::config::ControllerConfig,
    ) -> Option<Self> {
        let swarm_config: SwarmControllerConfig =
            match serde_json::from_value(config.config.clone()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to parse swarm controller config"
                    );
                    return None;
                }
            };

        let public_key = match SwarmPublicKey::from_config(&swarm_config.public_key) {
            Ok(pk) => pk,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "failed to parse swarm public key"
                );
                return None;
            }
        };

        Some(Self {
            config: swarm_config,
            public_key,
        })
    }
}

#[async_trait::async_trait]
impl super::Controller for SwarmController {
    fn name(&self) -> &str {
        "swarm"
    }

    async fn run(
        &self,
        settings: &Settings,
        registry: &Arc<ClientRegistry>,
    ) -> crate::Result<()> {
        // ── 1. BUILD AGENT ──
        let client_handle = registry.get_default();
        let mut agent = super::build_agent(
            settings,
            None,
            super::AgentMode::Private,
            client_handle,
            registry,
            None,
        )
        .await?;

        // ── 2. PROBE HARDWARE ──
        let node_name = self.config.node_name_or_default();
        let tool_names = agent.tool_names();
        let manifest = HardwareProbe::run(&node_name, tool_names).await;

        tracing::info!(
            node = %manifest.node_name,
            gpus = manifest.hardware.gpus.len(),
            cpus = manifest.hardware.cpus.len(),
            ram_mb = manifest.hardware.ram_bytes / (1024 * 1024),
            tools = manifest.capabilities.len(),
            "hardware probe complete"
        );

        // ── 3. CONNECT & REGISTER ──
        let mut conn = SwarmConnection::new(&self.config.url);

        let reg = conn.register(&manifest).await?;
        tracing::info!(
            node_id = %reg.node_id,
            "registered with swarm hub"
        );

        // ── 4. OPEN SSE STREAM ──
        let mut events = conn.open_event_stream().await?;

        // ── 5. HEARTBEAT (background) ──
        let heartbeat_conn = conn.clone();
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                if let Err(e) = heartbeat_conn
                    .heartbeat(&NodeStatus::Idle)
                    .await
                {
                    tracing::warn!(error = %e, "heartbeat failed");
                }
            }
        });

        // ── 6. TASK LOOP ──
        while let Some(event_result) = events.recv().await {
            match event_result {
                Ok(SwarmEvent::Task(wire_bytes)) => {
                    // Verify signature.
                    let payload_bytes = match verify_signed_payload(
                        &wire_bytes,
                        &self.public_key,
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "rejected task: signature verification failed"
                            );
                            continue;
                        }
                    };

                    // Parse task.
                    let task: SwarmTask = match serde_json::from_slice(payload_bytes) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "rejected task: invalid JSON payload"
                            );
                            continue;
                        }
                    };

                    tracing::info!(
                        task_id = %task.task_id,
                        prompt_len = task.prompt.len(),
                        payloads = task.payloads.len(),
                        "executing swarm task"
                    );

                    // Send busy status.
                    let _ = conn
                        .heartbeat(&NodeStatus::Busy {
                            task_id: task.task_id.clone(),
                        })
                        .await;

                    // Execute the task.
                    let result = execute_task(&mut agent, &conn, &task).await;

                    // Send result.
                    if let Err(e) = conn.send_result(&result).await {
                        tracing::error!(
                            task_id = %task.task_id,
                            error = %e,
                            "failed to send task result"
                        );
                    }

                    // Send idle status.
                    let _ = conn.heartbeat(&NodeStatus::Idle).await;

                    // Reset conversation for next task.
                    agent.clear();
                }
                Ok(SwarmEvent::Registered { node_id }) => {
                    tracing::info!(node_id = %node_id, "registration confirmed via SSE");
                }
                Ok(SwarmEvent::HeartbeatAck) => {
                    tracing::trace!("heartbeat acknowledged");
                }
                Ok(SwarmEvent::Shutdown) => {
                    tracing::info!("hub requested shutdown");
                    break;
                }
                Err(e) => {
                    tracing::error!(error = %e, "SSE stream error");
                    // TODO: reconnect with backoff
                    break;
                }
            }
        }

        // ── 7. CLEANUP ──
        heartbeat_handle.abort();
        tracing::info!("swarm controller shut down");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Task execution
// ---------------------------------------------------------------------------

/// Execute a single swarm task: fetch payloads, run agent, collect results.
async fn execute_task(
    agent: &mut crate::agent::Agent,
    conn: &SwarmConnection,
    task: &SwarmTask,
) -> SwarmResult {
    let start = Instant::now();

    // Fetch any ref payloads and verify their hashes.
    let payload_context = match fetch_and_verify_payloads(conn, &task.payloads).await {
        Ok(ctx) => ctx,
        Err(e) => {
            return SwarmResult {
                task_id: task.task_id.clone(),
                text: String::new(),
                payloads: vec![],
                status: TaskStatus::Failed {
                    error: format!("payload fetch failed: {e}"),
                },
                duration_secs: start.elapsed().as_secs(),
            };
        }
    };

    // Build the prompt, including payload context if any.
    let prompt = if payload_context.is_empty() {
        task.prompt.clone()
    } else {
        format!("{}\n\n{}", task.prompt, payload_context)
    };

    // Run the agent with a timeout if specified.
    let mut output = CaptureOutput::new();
    let agent_result = if let Some(timeout_secs) = task.timeout_secs {
        tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            agent.run(&prompt, &mut output),
        )
        .await
    } else {
        Ok(agent.run(&prompt, &mut output).await)
    };

    let (text, status) = match agent_result {
        Ok(Ok(text)) => (text, TaskStatus::Completed),
        Ok(Err(e)) => (
            String::new(),
            TaskStatus::Failed {
                error: format!("agent error: {e}"),
            },
        ),
        Err(_) => (
            output.text().to_string(),
            TaskStatus::Failed {
                error: "task timed out".into(),
            },
        ),
    };

    SwarmResult {
        task_id: task.task_id.clone(),
        text,
        payloads: vec![],
        status,
        duration_secs: start.elapsed().as_secs(),
    }
}

/// Fetch ref payloads from the hub, verify SHA-256 hashes.
///
/// Returns a string describing the payloads (for the agent's context),
/// or an error if any payload fails to download or verify.
async fn fetch_and_verify_payloads(
    conn: &SwarmConnection,
    payloads: &[Payload],
) -> crate::Result<String> {
    let mut context_lines = Vec::new();

    for payload in payloads {
        match payload {
            Payload::Inline { name, data } => {
                let text = String::from_utf8_lossy(data);
                context_lines.push(format!(
                    "Attached file '{name}' ({} bytes):\n{text}",
                    data.len()
                ));
            }
            Payload::Ref(blob_ref) => {
                tracing::info!(
                    name = %blob_ref.name,
                    sha256 = %blob_ref.sha256,
                    size = blob_ref.size,
                    "fetching blob payload"
                );

                let data = conn.fetch_blob(&blob_ref.sha256).await?;

                // Verify hash.
                let mut hasher = Sha256::new();
                hasher.update(&data);
                let hash = format!("{:x}", hasher.finalize());

                if hash != blob_ref.sha256 {
                    return Err(DysonError::Swarm(format!(
                        "blob hash mismatch for '{}': expected {}, got {hash}",
                        blob_ref.name, blob_ref.sha256
                    )));
                }

                context_lines.push(format!(
                    "Attached file '{}' ({} bytes, SHA-256 verified)",
                    blob_ref.name, data.len()
                ));
            }
        }
    }

    Ok(context_lines.join("\n\n"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::types::BlobRef;

    #[test]
    fn swarm_controller_config_parsing() {
        let json = serde_json::json!({
            "type": "swarm",
            "url": "https://hub.example.com",
            "public_key": "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "node_name": "test-node"
        });

        let config: SwarmControllerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.url, "https://hub.example.com");
        assert_eq!(config.node_name, Some("test-node".into()));
        assert_eq!(config.node_name_or_default(), "test-node");
    }

    #[test]
    fn swarm_controller_config_defaults_node_name() {
        let json = serde_json::json!({
            "type": "swarm",
            "url": "https://hub.example.com",
            "public_key": "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        });

        let config: SwarmControllerConfig = serde_json::from_value(json).unwrap();
        assert!(config.node_name.is_none());
        // node_name_or_default will return something (hostname or "unknown").
        let name = config.node_name_or_default();
        assert!(!name.is_empty());
    }

    #[test]
    fn swarm_controller_config_missing_url_fails() {
        let json = serde_json::json!({
            "type": "swarm",
            "public_key": "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        });

        let result: Result<SwarmControllerConfig, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn swarm_controller_config_missing_public_key_fails() {
        let json = serde_json::json!({
            "type": "swarm",
            "url": "https://hub.example.com"
        });

        let result: Result<SwarmControllerConfig, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_verify_inline_payload() {
        let payloads = vec![Payload::Inline {
            name: "config.yaml".into(),
            data: b"key: value".to_vec(),
        }];

        // No connection needed for inline payloads, but we need a conn.
        // We test the logic by calling fetch_and_verify_payloads which
        // only uses conn for Ref payloads.
        let conn = SwarmConnection::new("http://localhost:0");
        let ctx = fetch_and_verify_payloads(&conn, &payloads).await.unwrap();

        assert!(ctx.contains("config.yaml"));
        assert!(ctx.contains("key: value"));
    }
}
