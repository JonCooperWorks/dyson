// ===========================================================================
// Swarm types — data structures for the swarm resource routing protocol.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the core data types exchanged between Dyson nodes and a swarm
//   hub.  These types represent node hardware manifests, task requests,
//   task results, and the payload system (small inline vs large
//   content-addressed blobs).
//
// Payload tiers:
//
//   Small payloads (< 64KB) travel inline inside signed envelopes.
//   Large payloads are referenced by SHA-256 hash and transferred
//   separately over the WebSocket connection.  The hash is included
//   in the signed envelope, so integrity is guaranteed end-to-end:
//
//     private_key signs → envelope (contains SHA-256 hashes)
//     SHA-256 hashes verify → large payloads (transferred separately)
//
// Wire format:
//
//   All types serialize to canonical JSON via serde.  The JSON bytes
//   are what get signed (for tasks from the hub) or hashed (for blob
//   integrity).  No binary encoding — JSON is debuggable and the MCP
//   transport already speaks JSON-RPC.
// ===========================================================================

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Payload types — inline vs content-addressed reference
// ---------------------------------------------------------------------------

/// A content-addressed blob reference.
///
/// The SHA-256 hash is the identity.  The size lets the receiver
/// pre-allocate or reject too-large transfers before downloading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    /// SHA-256 hash of the raw bytes (hex-encoded).
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Human-readable name (for the agent's context, not for routing).
    pub name: String,
}

/// A payload: either inline bytes or a reference to fetch separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Payload {
    /// Small payload, inline.  Data is base64-encoded in JSON.
    Inline {
        name: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Large payload.  Fetch by hash from the hub, verify before use.
    Ref(BlobRef),
}

impl Payload {
    /// The human-readable name of this payload.
    pub fn name(&self) -> &str {
        match self {
            Self::Inline { name, .. } => name,
            Self::Ref(r) => &r.name,
        }
    }
}

// ---------------------------------------------------------------------------
// Node manifest — what a node reports about itself
// ---------------------------------------------------------------------------

/// Hardware and capability manifest sent during node registration.
///
/// The hub uses this to make routing decisions: which node gets which
/// task based on GPU availability, RAM, loaded tools, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeManifest {
    /// Human-readable node name (from config or hostname).
    pub node_name: String,
    /// Operating system (e.g. "linux", "macos").
    pub os: String,
    /// Detected hardware on this machine.
    pub hardware: HardwareInfo,
    /// Tool/skill names loaded on this node's agent.
    pub capabilities: Vec<String>,
    /// Optional plain-text description of this node's specialisations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Current node status.
    pub status: NodeStatus,
}

/// Hardware information detected by the probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareInfo {
    pub cpus: Vec<CpuInfo>,
    pub gpus: Vec<GpuInfo>,
    /// Total RAM in bytes.
    pub ram_bytes: u64,
    /// Free disk space in bytes (on the working directory's filesystem).
    pub disk_free_bytes: u64,
}

/// CPU information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    pub model: String,
    /// Logical core count (hardware threads, post-SMT/HT).
    pub cores: u32,
    /// Physical core count for this model, when the probe can determine it.
    ///
    /// On Linux this is the number of unique `(physical id, core id)` pairs
    /// reported in `/proc/cpuinfo`.  On macOS it's `hw.physicalcpu` (the sum
    /// of performance + efficiency cores on Apple Silicon).  `None` when the
    /// platform doesn't expose it.
    #[serde(default)]
    pub physical_cores: Option<u32>,
}

/// GPU information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub model: String,
    /// Total VRAM in bytes.
    pub vram_bytes: u64,
    pub driver: String,
    /// GPU core count, when reported by the platform.
    ///
    /// Apple Silicon GPUs surface this via `system_profiler` as `sppci_cores`
    /// (e.g. 38 for an M2 Max).  `None` when the probe cannot determine it
    /// (discrete NVIDIA GPUs, Linux, etc.).
    #[serde(default)]
    pub cores: Option<u32>,
}

/// Current status of a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NodeStatus {
    /// Ready to accept tasks.
    Idle,
    /// Currently executing a task.
    Busy {
        task_id: String,
    },
    /// Finishing current task, won't accept new ones (graceful shutdown).
    Draining,
}

// ---------------------------------------------------------------------------
// Task — what the hub sends to a node
// ---------------------------------------------------------------------------

/// An inbound task from the hub to a node.
///
/// This is the payload inside the signed envelope.  The node verifies
/// the signature, fetches any referenced blobs, then feeds the prompt
/// to its agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmTask {
    /// Unique task ID (assigned by the hub).
    pub task_id: String,
    /// The prompt the agent should execute.
    pub prompt: String,
    /// Attached payloads (datasets, configs, files).
    #[serde(default)]
    pub payloads: Vec<Payload>,
    /// Optional timeout in seconds.  `None` = no timeout.
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Result — what a node sends back to the hub
// ---------------------------------------------------------------------------

/// A task result from a node back to the hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmResult {
    /// The task this result is for.
    pub task_id: String,
    /// The agent's final text output.
    pub text: String,
    /// Any files produced (same inline/ref split as inbound).
    #[serde(default)]
    pub payloads: Vec<Payload>,
    /// How the task ended.
    pub status: TaskStatus,
    /// Wall-clock duration in seconds.
    pub duration_secs: u64,
}

/// How a task ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    Failed { error: String },
    Cancelled,
}

// ---------------------------------------------------------------------------
// TaskCheckpoint — mid-task progress event emitted by a node
// ---------------------------------------------------------------------------

/// A progress/checkpoint event emitted by a node while a task is still running.
///
/// Long-running tasks (model fine-tuning, data crunching, etc.) send these
/// to the hub via `POST /swarm/checkpoint` so callers polling
/// `swarm_task_status` / `swarm_task_checkpoints` can observe progress
/// without waiting for the final `SwarmResult`.
///
/// Checkpoints carry metadata only — no payloads.  If a task needs to
/// deliver intermediate artifacts, it should emit them via the final
/// `SwarmResult.payloads` when the task completes, or (in a future
/// revision) a dedicated artifact event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    /// The task this checkpoint belongs to.
    pub task_id: String,
    /// Monotonic sequence number, per task.  Starts at 1, incremented on
    /// each emit.  Callers use this with `swarm_task_checkpoints`'s
    /// `since_sequence` to fetch only new events.
    pub sequence: u32,
    /// Human-readable progress note.
    pub message: String,
    /// Optional fractional progress (0.0..=1.0).  Absent when the task
    /// can't estimate a percentage.
    #[serde(default)]
    pub progress: Option<f32>,
    /// Seconds elapsed on the node since task execution started.
    pub emitted_at_secs: u64,
}

// ---------------------------------------------------------------------------
// Base64 serde helper — encode Vec<u8> as base64 in JSON
// ---------------------------------------------------------------------------

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_inline_roundtrip() {
        let payload = Payload::Inline {
            name: "config.yaml".into(),
            data: b"key: value\n".to_vec(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: Payload = serde_json::from_str(&json).unwrap();
        match parsed {
            Payload::Inline { name, data } => {
                assert_eq!(name, "config.yaml");
                assert_eq!(data, b"key: value\n");
            }
            _ => panic!("expected Inline"),
        }
    }

    #[test]
    fn payload_ref_roundtrip() {
        let payload = Payload::Ref(BlobRef {
            sha256: "abcdef1234567890".into(),
            size: 1024,
            name: "dataset.json".into(),
        });
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: Payload = serde_json::from_str(&json).unwrap();
        match parsed {
            Payload::Ref(r) => {
                assert_eq!(r.sha256, "abcdef1234567890");
                assert_eq!(r.size, 1024);
                assert_eq!(r.name, "dataset.json");
            }
            _ => panic!("expected Ref"),
        }
    }

    #[test]
    fn swarm_task_roundtrip() {
        let task = SwarmTask {
            task_id: "test-123".into(),
            prompt: "fine-tune the model".into(),
            payloads: vec![],
            timeout_secs: Some(3600),
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: SwarmTask = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "test-123");
        assert_eq!(parsed.timeout_secs, Some(3600));
    }

    #[test]
    fn task_checkpoint_roundtrip() {
        let cp = TaskCheckpoint {
            task_id: "task-abc".into(),
            sequence: 7,
            message: "epoch 3/10 complete".into(),
            progress: Some(0.3),
            emitted_at_secs: 420,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let parsed: TaskCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "task-abc");
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.message, "epoch 3/10 complete");
        assert_eq!(parsed.progress, Some(0.3));
        assert_eq!(parsed.emitted_at_secs, 420);
    }

    #[test]
    fn task_checkpoint_progress_optional() {
        let json = r#"{"task_id":"t","sequence":1,"message":"hi","emitted_at_secs":0}"#;
        let parsed: TaskCheckpoint = serde_json::from_str(json).unwrap();
        assert!(parsed.progress.is_none());
    }

    #[test]
    fn node_status_serde() {
        let idle = NodeStatus::Idle;
        let json = serde_json::to_string(&idle).unwrap();
        assert!(json.contains("\"status\":\"idle\""));

        let busy = NodeStatus::Busy {
            task_id: "abc".into(),
        };
        let json = serde_json::to_string(&busy).unwrap();
        assert!(json.contains("\"task_id\":\"abc\""));
    }
}
