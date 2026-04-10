// ===========================================================================
// Swarm — resource-aware task routing across Dyson nodes.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Implements the Dyson side of swarm participation.  When a swarm
//   controller is configured, this node joins a swarm hub and becomes
//   both a task executor (receives work via SSE) and a task submitter
//   (dispatches work via auto-wired MCP tools).
//
// Module layout:
//   mod.rs          — module exports (this file)
//   types.rs        — NodeManifest, SwarmTask, SwarmResult, Payload
//   verify.rs       — Ed25519 signature verification (V1, no agility)
//   probe.rs        — HardwareProbe (GPU/CPU/RAM/disk detection)
//   connection.rs   — SwarmConnection (SSE inbound, POST outbound)
//
// How swarm fits into the architecture:
//
//   dyson.json "controllers" array
//     │
//     ├── { "type": "swarm", "url": "...", "public_key": "v1:..." }
//     │
//     ▼
//   listen.rs sees "swarm" controller config
//     │
//     ├── Auto-inject hub URL as MCP skill into settings
//     │   → ALL agents get swarm_dispatch, swarm_status tools
//     │
//     └── Create SwarmController
//           │
//           ├── build_agent() with shared ClientRegistry
//           ├── HardwareProbe → NodeManifest
//           ├── POST /swarm/register
//           ├── GET /swarm/events (SSE stream)
//           ├── Heartbeat background task
//           │
//           └── Loop: receive task → verify sig → fetch blobs
//                     → agent.run() → POST /swarm/result
//
// Social contract:
//   You can use the swarm (dispatch tasks) because you are part of
//   the swarm (accept tasks).  Adding the controller is the opt-in.
//   The public key ensures only the legitimate hub can drive you.
//
// Protocols:
//   - Inbound (hub → node): SSE on GET {url}/swarm/events
//   - Outbound (node → hub): POST to /swarm/register, /heartbeat, /result, /blob
//   - Tool calls (any agent → hub): MCP on {url}/mcp (auto-wired McpSkill)
//
// Signatures:
//   V1 = Ed25519 (via ring).  No algorithmic agility.
//   Version bump to change algorithm.  The public_key config
//   encodes the version: "v1:base64...".
// ===========================================================================

pub mod connection;
pub mod probe;
pub mod types;
pub mod verify;
