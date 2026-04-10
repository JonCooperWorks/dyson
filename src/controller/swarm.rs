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
//     3. Connect, register, open SSE (with reconnection on failure)
//     4. Spawn heartbeat background task
//     5. Loop: SSE event → verify → fetch blobs → agent.run() → POST result
//     6. On SSE disconnect: reconnect with exponential backoff
//     7. On hub shutdown or fatal error: exit
//
// Social contract:
//   Adding this controller means "I can use the swarm, and the swarm
//   can use me."  The public key ensures only the configured hub can
//   send tasks.
// ===========================================================================

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config::{Settings, SwarmControllerConfig};
use crate::controller::{ClientRegistry, Output};
use crate::error::DysonError;
use crate::swarm::connection::{SwarmConnection, SwarmEvent};
use crate::swarm::probe::HardwareProbe;
use crate::swarm::types::{
    BlobRef, NodeManifest, NodeStatus, Payload, SwarmResult, SwarmTask, TaskStatus,
};
use crate::swarm::verify::{SwarmPublicKey, verify_signed_payload};
use crate::tool::ToolOutput;

/// Delay between heartbeats.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum consecutive reconnection attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Base delay for exponential backoff on reconnection (doubled each attempt).
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);

/// Maximum size for inline result payloads (64 KiB).
const INLINE_THRESHOLD: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// SwarmCaptureOutput — collects text + file paths from the agent
// ---------------------------------------------------------------------------

/// Output implementation that captures text and file paths.
///
/// Like `CaptureOutput` from subagents, but also records file paths
/// so swarm results can include them as payloads.
struct SwarmCaptureOutput {
    text: String,
    files: Vec<PathBuf>,
}

impl SwarmCaptureOutput {
    fn new() -> Self {
        Self {
            text: String::new(),
            files: Vec::new(),
        }
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn take_files(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.files)
    }

    fn clear(&mut self) {
        self.text.clear();
        self.files.clear();
    }
}

impl Output for SwarmCaptureOutput {
    fn text_delta(&mut self, text: &str) -> Result<(), DysonError> {
        self.text.push_str(text);
        Ok(())
    }

    fn tool_use_start(&mut self, _id: &str, _name: &str) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, _output: &ToolOutput) -> Result<(), DysonError> {
        Ok(())
    }

    fn send_file(&mut self, path: &std::path::Path) -> Result<(), DysonError> {
        self.files.push(path.to_path_buf());
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        tracing::warn!(error = %error, "swarm task agent error");
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        Ok(())
    }
}

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
                    tracing::error!(error = %e, "failed to parse swarm controller config");
                    return None;
                }
            };

        let public_key = match SwarmPublicKey::from_config(&swarm_config.public_key) {
            Ok(pk) => pk,
            Err(e) => {
                tracing::error!(error = %e, "failed to parse swarm public key");
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

        // ── 3. CONNECT WITH RECONNECTION ──
        let status = Arc::new(Mutex::new(NodeStatus::Idle));
        let mut output = SwarmCaptureOutput::new();

        // Outer loop: reconnect on SSE failures.
        let mut consecutive_failures: u32 = 0;

        loop {
            match self
                .run_session(&manifest, &mut agent, &mut output, &status)
                .await
            {
                SessionResult::HubShutdown => {
                    tracing::info!("hub requested shutdown");
                    break;
                }
                SessionResult::Disconnected(e) => {
                    consecutive_failures += 1;

                    if consecutive_failures > MAX_RECONNECT_ATTEMPTS {
                        tracing::error!(
                            attempts = consecutive_failures,
                            "max reconnection attempts exceeded — giving up"
                        );
                        return Err(DysonError::Swarm(
                            "max reconnection attempts exceeded".into(),
                        ));
                    }

                    let delay = RECONNECT_BASE_DELAY * 2u32.saturating_pow(consecutive_failures - 1);
                    let delay = delay.min(Duration::from_secs(60));

                    tracing::warn!(
                        error = %e,
                        attempt = consecutive_failures,
                        retry_secs = delay.as_secs(),
                        "SSE disconnected — reconnecting"
                    );

                    tokio::time::sleep(delay).await;
                }
            }
        }

        tracing::info!("swarm controller shut down");
        Ok(())
    }
}

/// Result of a single SSE session.
enum SessionResult {
    /// Hub sent a shutdown event.
    HubShutdown,
    /// SSE stream disconnected (retryable).
    Disconnected(DysonError),
}

impl SwarmController {
    /// Run a single SSE session: register, connect, process events.
    ///
    /// Returns when the session ends (disconnect, shutdown, or error).
    async fn run_session(
        &self,
        manifest: &NodeManifest,
        agent: &mut crate::agent::Agent,
        output: &mut SwarmCaptureOutput,
        status: &Arc<Mutex<NodeStatus>>,
    ) -> SessionResult {
        // Connect and register.
        let mut conn = SwarmConnection::new(&self.config.url);

        let reg = match conn.register(manifest).await {
            Ok(r) => r,
            Err(e) => return SessionResult::Disconnected(e),
        };

        tracing::info!(node_id = %reg.node_id, "registered with swarm hub");

        // Open SSE stream.
        let mut events = match conn.open_event_stream().await {
            Ok(rx) => rx,
            Err(e) => return SessionResult::Disconnected(e),
        };

        // Heartbeat background task.
        let heartbeat_conn = conn.clone();
        let heartbeat_status = Arc::clone(status);
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let current = heartbeat_status.lock().await.clone();
                if let Err(e) = heartbeat_conn.heartbeat(&current).await {
                    tracing::warn!(error = %e, "heartbeat failed");
                }
            }
        });

        // Event loop.
        let result = loop {
            let event_result = match events.recv().await {
                Some(r) => r,
                None => break SessionResult::Disconnected(
                    DysonError::Swarm("SSE channel closed".into()),
                ),
            };

            match event_result {
                Ok(SwarmEvent::Task(wire_bytes)) => {
                    // Verify signature.
                    let payload_bytes = match verify_signed_payload(
                        &wire_bytes,
                        &self.public_key,
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(error = %e, "rejected task: bad signature");
                            continue;
                        }
                    };

                    // Parse task.
                    let task: SwarmTask = match serde_json::from_slice(payload_bytes) {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!(error = %e, "rejected task: invalid JSON");
                            continue;
                        }
                    };

                    tracing::info!(
                        task_id = %task.task_id,
                        payloads = task.payloads.len(),
                        "executing swarm task"
                    );

                    // Mark busy.
                    *status.lock().await = NodeStatus::Busy {
                        task_id: task.task_id.clone(),
                    };

                    // Execute.
                    let result = execute_task(agent, &conn, &task, output).await;

                    // Send result.
                    if let Err(e) = conn.send_result(&result).await {
                        tracing::error!(
                            task_id = %task.task_id,
                            error = %e,
                            "failed to send task result"
                        );
                    }

                    // Reset for next task.
                    *status.lock().await = NodeStatus::Idle;
                    agent.clear();
                    output.clear();
                }
                Ok(SwarmEvent::Registered { node_id }) => {
                    tracing::info!(node_id = %node_id, "registration confirmed via SSE");
                }
                Ok(SwarmEvent::HeartbeatAck) => {
                    tracing::trace!("heartbeat acknowledged");
                }
                Ok(SwarmEvent::Shutdown) => {
                    break SessionResult::HubShutdown;
                }
                Err(e) => {
                    break SessionResult::Disconnected(e);
                }
            }
        };

        heartbeat_handle.abort();

        result
    }
}

// ---------------------------------------------------------------------------
// Task execution
// ---------------------------------------------------------------------------

/// Execute a single swarm task: fetch payloads, run agent, collect result files.
async fn execute_task(
    agent: &mut crate::agent::Agent,
    conn: &SwarmConnection,
    task: &SwarmTask,
    output: &mut SwarmCaptureOutput,
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
    let agent_result = if let Some(timeout_secs) = task.timeout_secs {
        tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            agent.run(&prompt, output),
        )
        .await
    } else {
        Ok(agent.run(&prompt, output).await)
    };

    let (text, task_status) = match agent_result {
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

    // Collect result files and upload large ones.
    let files = output.take_files();
    let payloads = collect_result_payloads(conn, &files).await;

    SwarmResult {
        task_id: task.task_id.clone(),
        text,
        payloads,
        status: task_status,
        duration_secs: start.elapsed().as_secs(),
    }
}

/// Read result files produced by the agent, split into inline/ref payloads.
///
/// Small files (< 64 KiB) are inlined.  Large files are hashed, uploaded
/// to the hub, and referenced by SHA-256.  Files that can't be read are
/// logged and skipped.
async fn collect_result_payloads(conn: &SwarmConnection, files: &[PathBuf]) -> Vec<Payload> {
    let mut payloads = Vec::new();

    for path in files {
        let data = match tokio::fs::read(path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping result file: could not read"
                );
                continue;
            }
        };

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        if data.len() <= INLINE_THRESHOLD {
            payloads.push(Payload::Inline { name, data });
        } else {
            // Hash, upload, reference.
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let sha256 = format!("{:x}", hasher.finalize());

            if let Err(e) = conn.upload_blob(&sha256, &data).await {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping result file: upload failed"
                );
                continue;
            }

            payloads.push(Payload::Ref(BlobRef {
                sha256,
                size: data.len() as u64,
                name,
            }));
        }
    }

    payloads
}

/// Fetch ref payloads from the hub, verify SHA-256 hashes.
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

        let conn = SwarmConnection::new("http://localhost:0");
        let ctx = fetch_and_verify_payloads(&conn, &payloads).await.unwrap();

        assert!(ctx.contains("config.yaml"));
        assert!(ctx.contains("key: value"));
    }

    #[test]
    fn swarm_capture_output_collects_text_and_files() {
        let mut output = SwarmCaptureOutput::new();

        output.text_delta("hello ").unwrap();
        output.text_delta("world").unwrap();
        output.send_file(std::path::Path::new("/tmp/report.pdf")).unwrap();
        output.send_file(std::path::Path::new("/tmp/data.csv")).unwrap();

        assert_eq!(output.text(), "hello world");
        let files = output.take_files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], PathBuf::from("/tmp/report.pdf"));
        assert_eq!(files[1], PathBuf::from("/tmp/data.csv"));

        // take_files drains.
        assert!(output.take_files().is_empty());
    }

    #[test]
    fn swarm_capture_output_clear() {
        let mut output = SwarmCaptureOutput::new();

        output.text_delta("text").unwrap();
        output.send_file(std::path::Path::new("/tmp/file")).unwrap();

        output.clear();
        assert!(output.text().is_empty());
        assert!(output.take_files().is_empty());
    }

    #[tokio::test]
    async fn collect_result_payloads_inline_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("small.txt");
        tokio::fs::write(&file_path, b"hello").await.unwrap();

        let conn = SwarmConnection::new("http://localhost:0");
        let payloads = collect_result_payloads(&conn, &[file_path]).await;

        assert_eq!(payloads.len(), 1);
        match &payloads[0] {
            Payload::Inline { name, data } => {
                assert_eq!(name, "small.txt");
                assert_eq!(data, b"hello");
            }
            _ => panic!("expected Inline"),
        }
    }

    #[tokio::test]
    async fn collect_result_payloads_skips_missing_file() {
        let conn = SwarmConnection::new("http://localhost:0");
        let payloads =
            collect_result_payloads(&conn, &[PathBuf::from("/nonexistent/file.txt")]).await;

        assert!(payloads.is_empty());
    }
}
