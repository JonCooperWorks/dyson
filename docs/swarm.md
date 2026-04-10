# Swarm

The swarm controller enables dynamic resource routing across Dyson nodes.
Adding a swarm controller makes this Dyson both a **worker** (the hub can
send it tasks) and a **client** (its agent can dispatch tasks to other nodes).
Participation is symmetric: you use the swarm because you are part of it.

**Key files:**
- `src/swarm/types.rs` — `NodeManifest`, `SwarmTask`, `SwarmResult`, `Payload`
- `src/swarm/verify.rs` — `SwarmPublicKey`, `verify_signed_payload()` (V1 Ed25519)
- `src/swarm/probe.rs` — `HardwareProbe` (GPU/CPU/RAM/disk detection)
- `src/swarm/connection.rs` — `SwarmConnection` (SSE inbound, POST outbound)
- `src/controller/swarm.rs` — `SwarmController`
- `src/config/mod.rs` — `SwarmControllerConfig`
- `src/command/listen.rs` — Controller factory + MCP auto-wiring

---

## Configuration

```json
{
  "controllers": [
    { "type": "terminal" },
    {
      "type": "swarm",
      "url": "https://hub.example.com",
      "public_key": "v1:K2dYr0base64encodedkey...",
      "node_name": "gpu-workstation-01"
    }
  ]
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `url` | yes | — | Base URL of the swarm hub |
| `public_key` | yes | — | Versioned Ed25519 public key for task verification (`v1:base64...`) |
| `node_name` | no | derived from URL | Human-readable name for this node in the registry |

---

## Architecture

```
┌────────────────────────────────────────────────────────┐
│                     SWARM HUB                          │
│                                                        │
│  /mcp             ← MCP server (tool discovery)        │
│  /swarm/register  ← POST (node registration)           │
│  /swarm/events    ← SSE (push tasks to nodes)          │
│  /swarm/heartbeat ← POST (node status updates)         │
│  /swarm/result    ← POST (task results from nodes)     │
│  /swarm/blob/{sha256} ← GET/PUT (large payloads)       │
│                                                        │
└──────┬──────────┬──────────────────────┬───────────────┘
       │ SSE      │ SSE                  │ MCP
       ▼          ▼                      ▼
   ┌────────┐ ┌────────┐          ┌────────────┐
   │ Node A │ │ Node B │          │ Any Dyson  │
   │ (GPU)  │ │ (CPU)  │          │ (client)   │
   │        │ │        │          │            │
   │ Swarm  │ │ Swarm  │          │ LLM calls: │
   │ Ctrl   │ │ Ctrl   │          │ swarm_     │
   │        │ │        │          │ dispatch   │
   └────────┘ └────────┘          └────────────┘
    Workers                        Task submitter
    (receive + execute)            (via auto-wired MCP)
```

Two protocols:

- **SSE** (hub → node): The hub pushes signed tasks to connected nodes.
- **POST** (node → hub): Nodes send registration, heartbeats, and results.
- **MCP** (any agent → hub): Auto-wired so all agents get `swarm_dispatch` and other hub-defined tools.

---

## The Hub (Server)

The hub lives at [`crates/swarm/`](../crates/swarm/) and ships as a binary
named `swarm`.  It is an in-memory, tokio-based HTTP server responsible for:

1. **Node registry** — Accepting `POST /swarm/register` with a `NodeManifest`,
   assigning a `node_id` and auth token.

2. **Constraint matching** — When a task is submitted (via MCP `swarm_dispatch`),
   the hub selects the best node based on hardware (GPU, RAM), capabilities
   (loaded tools), and current status (idle vs busy).

3. **Task dispatch** — Signing the `SwarmTask` with its Ed25519 private key and
   pushing it to the selected node via SSE (`event: task`).

4. **Blob storage** — Serving large payloads (`GET /swarm/blob/{sha256}`) and
   accepting result payloads (`PUT /swarm/blob/{sha256}`).

5. **Health monitoring** — Receiving heartbeats from nodes, reaping stale entries.

### Running the hub

```bash
# First run: generates a fresh signing key and prints the public key
# string you drop into node dyson.json configs.
cargo run -p swarm -- --bind 0.0.0.0:8080 --data-dir ./hub-data
```

CLI flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `127.0.0.1:8080` | HTTP listen address |
| `--data-dir` | `./hub-data` | Where `hub.key` and `blobs/` live |
| `--heartbeat-timeout-secs` | `90` | Reap nodes whose last heartbeat is older than this |
| `--log-level` | `info` | `tracing` env filter |

On first run the hub generates a fresh PKCS#8 Ed25519 keypair under
`data_dir/hub.key` (chmod `0600`) and prints the public key in the
`"v1:..."` format node operators need:

```
Hub public key (add to node config): v1:K2dYr0base64encodedkey...
```

Subsequent runs load the existing key silently.  State is ephemeral: a
hub restart forgets every registered node and every in-flight task.

### Hub endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/mcp` | POST | MCP JSON-RPC (tool discovery + task dispatch) |
| `/swarm/register` | POST | Node registration, returns `{ node_id, token }` |
| `/swarm/events` | GET | SSE stream for pushing tasks to a node |
| `/swarm/heartbeat` | POST | Node status update |
| `/swarm/result` | POST | Task result from a node |
| `/swarm/blob/{sha256}` | GET | Download a payload blob by hash |
| `/swarm/blob/{sha256}` | PUT | Upload a result payload blob |

### SSE event types

| Event | Data | Description |
|-------|------|-------------|
| `registered` | `{ "node_id": "..." }` | Registration confirmed |
| `task` | base64 of signed wire bytes | Execute this task |
| `heartbeat_ack` | `{}` | Heartbeat received |
| `shutdown` | `{}` | Graceful disconnect requested |

### Signing tasks

The hub signs every task with its Ed25519 private key. The wire format:

```
version (1 byte) || signature (64 bytes) || canonical JSON payload
```

V1 = Ed25519. No algorithmic agility. To change the algorithm, bump the
version. Nodes reject any version they don't recognize.

---

## The Node (Client)

The Dyson side. When `"type": "swarm"` appears in the controllers config,
Dyson does two things automatically:

### 1. Creates a SwarmController (worker)

The controller lifecycle:

```
SwarmController::run()
  ├── build_agent() with shared ClientRegistry
  ├── HardwareProbe::run() → NodeManifest
  │     ├── OS detection (compile-time)
  │     ├── GPU: nvidia-smi (Linux/macOS), system_profiler (macOS)
  │     ├── CPU: /proc/cpuinfo (Linux), sysctl (macOS)
  │     ├── RAM: /proc/meminfo (Linux), sysctl (macOS)
  │     ├── Disk: statvfs (Unix)
  │     └── Capabilities: agent.tool_names()
  │
  ├── POST /swarm/register → { node_id, token }
  ├── GET /swarm/events → open SSE stream
  ├── Spawn heartbeat task (POST every 30s)
  │
  └── Loop:
        ├── SSE "task" event received
        │     ├── Verify Ed25519 signature against public_key
        │     ├── Parse SwarmTask from JSON
        │     ├── Fetch ref payloads (GET /swarm/blob/{sha256})
        │     ├── Verify SHA-256 hash of each blob
        │     ├── agent.run(prompt) with optional timeout
        │     ├── POST /swarm/result
        │     └── agent.clear() (reset for next task)
        │
        ├── SSE "shutdown" → break
        └── SSE error → break (TODO: reconnect with backoff)
```

### 2. Auto-wires the hub as an MCP skill (client)

The hub URL is injected into `settings.skills` as an MCP server:

```rust
// listen.rs — when "swarm" controller is created:
settings.skills.push(SkillConfig::Mcp(McpConfig {
    name: "swarm_<node_name>",
    transport: McpTransportConfig::Http {
        url: "{hub_url}/mcp",
    },
}));
```

This means **every** agent on this Dyson (terminal, telegram, swarm) gets
the hub's MCP tools — typically `swarm_dispatch`, `swarm_status`,
`list_nodes`. The LLM uses them like any other tool.

---

## Payloads

Tasks and results can carry file attachments. Two tiers:

**Inline** — Small data (configs, prompts) travels inside the signed envelope:

```json
{ "type": "inline", "name": "config.yaml", "data": "base64..." }
```

**Ref** — Large data (datasets, model weights) is referenced by SHA-256 hash
and transferred separately:

```json
{ "type": "ref", "name": "dataset.json", "sha256": "a1b2c3...", "size": 2147483648 }
```

The signature covers the hashes, so tampering with referenced data breaks the
verification chain. The node fetches blobs via `GET /swarm/blob/{sha256}` and
verifies the hash before starting work.

---

## Signature Verification

No algorithmic agility. Each version specifies exactly one algorithm.

| Version | Algorithm | Key size | Signature size |
|---------|-----------|----------|----------------|
| V1 (`0x01`) | Ed25519 (RFC 8032) | 32 bytes | 64 bytes |

The public key in config encodes the version:

```
"public_key": "v1:K2dYr0base64encodedkey..."
              ───  ─────────────────────────
              version    32 bytes, base64
```

Verification:

1. Read version byte from wire message
2. Version doesn't match config key? **Reject.** No fallback.
3. Verify Ed25519 signature over the JSON payload bytes
4. Invalid? **Reject.**
5. Parse the now-trusted JSON as `SwarmTask`

---

## Node Manifest

Sent during registration. The hub uses this for routing decisions.

```json
{
  "node_name": "gpu-workstation-01",
  "os": "linux",
  "hardware": {
    "cpus": [{ "model": "AMD Ryzen 9 7950X", "cores": 32 }],
    "gpus": [{ "model": "NVIDIA RTX 4090", "vram_bytes": 25769803776, "driver": "560.35" }],
    "ram_bytes": 68719476736,
    "disk_free_bytes": 500000000000
  },
  "capabilities": ["bash", "read_file", "web_search", "mcp__github__create_pull_request"],
  "status": { "status": "idle" }
}
```

Hardware detection is conditional-compiled per OS. Capabilities are the
agent's loaded tool names — the hub can route "needs bash" or "needs
web_search" tasks to nodes that have those tools.

---

## Testing

```bash
# All swarm tests (41 tests)
cargo test --lib swarm

# Controller tests (5 tests)
cargo test --lib controller::swarm
```

Tests cover:
- Ed25519 sign/verify roundtrip, tampered payload, tampered signature, wrong key, wrong version
- Public key parsing: valid, invalid version, bad base64, wrong length
- SSE event parsing: registered, task, heartbeat_ack, shutdown, unknown, incomplete, multiline
- nvidia-smi output parsing: single GPU, multiple GPUs, empty, malformed
- `/proc/cpuinfo` parsing: single model, mixed models, empty (Linux only)
- `/proc/meminfo` parsing: normal, missing, empty (Linux only)
- macOS VRAM string parsing, system_profiler JSON parsing (macOS only)
- SwarmControllerConfig: valid, defaults, missing required fields
- Inline payload fetch and verification
