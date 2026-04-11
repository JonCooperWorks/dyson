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
| `node_name` | no | `dyson-node-{hash}` | Human-readable name for this node (defaults to a deterministic hash of the hub URL) |

---

## Architecture

```
+--------------------------------------------------------+
|                     SWARM HUB                          |
|                                                        |
|  /mcp               <- MCP server (tool discovery)     |
|  /swarm/register    <- POST (node registration)        |
|  /swarm/events      <- SSE  (push tasks to nodes)      |
|  /swarm/heartbeat   <- POST (node status updates)      |
|  /swarm/result      <- POST (task results from nodes)  |
|  /swarm/checkpoint  <- POST (progress events from task)|
|  /swarm/blob/{sha}  <- GET/PUT (large payloads)        |
|                                                        |
+------+----------+----------------------+---------------+
       | SSE      | SSE                  | MCP
       v          v                      v
   +--------+ +--------+          +------------+
   | Node A | | Node B |          | Any Dyson  |
   | (GPU)  | | (CPU)  |          | (client)   |
   |        | |        |          |            |
   | Swarm  | | Swarm  |          | LLM calls: |
   | Ctrl   | | Ctrl   |          | swarm_     |
   |        | |        |          | dispatch   |
   +--------+ +--------+          +------------+
    Workers                        Task submitter
    (receive + execute)            (via auto-wired MCP)
```

Two protocols:

- **SSE** (hub -> node): The hub pushes signed tasks to connected nodes.
- **POST** (node -> hub): Nodes send registration, heartbeats, and results.
- **MCP** (any agent -> hub): Auto-wired so agents get `swarm_dispatch` and other hub-defined tools.

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

Generate a signing key first, then start the hub:

```bash
# Generate a signing key (one-time)
swarm-keygen --out ./hub-data/hub.key

# Start the hub (localhost — no TLS needed)
swarm --bind 127.0.0.1:8080 --data-dir ./hub-data

# Start on an external interface (TLS required)
swarm --bind 0.0.0.0:443 --data-dir ./hub-data \
      --cert cert.pem --private-key key.pem

# Or with Let's Encrypt
swarm --bind 0.0.0.0:443 --data-dir ./hub-data \
      --letsencrypt --domain hub.example.com
```

The hub prints the public key on startup:

```
Hub public key (add to node config): v1:K2dYr0base64encodedkey...
```

State is ephemeral: a hub restart forgets every registered node and
every in-flight task.

### CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--bind` | `127.0.0.1:8080` | HTTP(S) listen address |
| `--data-dir` | `./hub-data` | Where `hub.key` and `blobs/` live |
| `--heartbeat-timeout-secs` | `90` | Reap nodes whose last heartbeat is older than this |
| `--log-level` | `info` | `tracing` env filter |
| `--cert` | — | TLS certificate chain (PEM). Requires `--private-key` |
| `--private-key` | — | TLS private key (PEM). Requires `--cert` |
| `--letsencrypt` | — | Enable Let's Encrypt automatic TLS. Requires `--domain` |
| `--domain` | — | Domain name for Let's Encrypt |
| `--letsencrypt-email` | — | Contact email for Let's Encrypt registration |
| `--cert-cache-dir` | `.swarm-certs` | Directory to cache Let's Encrypt certificates |
| `--dangerous-no-tls` | — | Allow plain HTTP on non-localhost interfaces |
| `--dangerous-no-auth` | — | Allow running without authentication on non-localhost interfaces |

### TLS

TLS is **mandatory** when binding to a non-localhost address.  Localhost
(`127.0.0.1`, `::1`) skips TLS automatically.  Two TLS modes:

- **Manual**: provide `--cert` and `--private-key` PEM files.
- **Let's Encrypt**: provide `--letsencrypt` and `--domain`. Certificates
  are automatically provisioned via TLS-ALPN-01 (same port, no port 80
  needed) and cached in `--cert-cache-dir`.

Pass `--dangerous-no-tls` to explicitly serve plain HTTP on external
interfaces (not recommended).

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

The `/mcp` endpoint accepts an optional `?caller=<node_name>` query
parameter.  When set, `list_nodes` results exclude the calling node so
it only sees its peers.

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
  |-- build_agent() with shared ClientRegistry
  |     Excludes swarm_dispatch from MCP tools (prevents recursion)
  |     Injects controller prompt (instructs agent to use tools)
  |     Wraps list_nodes via ?caller= so agent only sees peers
  |
  |-- HardwareProbe::run() -> NodeManifest
  |     |-- OS detection (compile-time)
  |     |-- GPU: nvidia-smi (Linux/macOS), system_profiler (macOS)
  |     |-- CPU: /proc/cpuinfo (Linux), sysctl (macOS)
  |     |-- RAM: /proc/meminfo (Linux), sysctl (macOS)
  |     |-- Disk: statvfs (Unix)
  |     +-- Capabilities: agent.tool_names()
  |
  |-- POST /swarm/register -> { node_id, token }
  |-- GET /swarm/events -> open SSE stream
  |-- Spawn heartbeat task (POST every 15s)
  |
  +-- Loop:
        |-- SSE "task" event received
        |     |-- Verify Ed25519 signature against public_key
        |     |-- Parse SwarmTask from JSON
        |     |-- Fetch ref payloads (GET /swarm/blob/{sha256})
        |     |-- Verify SHA-256 hash of each blob
        |     |-- agent.run(prompt) with optional timeout
        |     |-- POST /swarm/result
        |     +-- agent.clear() (reset for next task)
        |
        |-- SSE "shutdown" -> break
        +-- SSE disconnect -> reconnect with exponential backoff
              (base 2s, capped at 60s, max 10 attempts)
```

### 2. Auto-wires the hub as an MCP skill (client)

The hub URL is injected into `settings.skills` as an MCP server:

```rust
// listen.rs — when "swarm" controller is created:
let node_name = swarm_config.node_name_or_default();
let hub_base = swarm_config.url.trim_end_matches('/');

settings.skills.push(SkillConfig::Mcp(Box::new(McpConfig {
    name: format!("swarm_{node_name}"),
    transport: McpTransportConfig::Http {
        url: format!("{hub_base}/mcp?caller={node_name}"),
        ..
    },
    exclude_tools: vec![],
})));
```

This means **every** agent on this Dyson (terminal, telegram, swarm) gets
the hub's MCP tools — `swarm_dispatch`, `swarm_submit`, `swarm_status`,
`list_nodes`, and the task-query tools below.

The swarm controller's own agent is special:
- `swarm_dispatch` is excluded (prevents recursive task loops)
- `list_nodes` results are filtered by the hub (via `?caller=`) so the
  agent never sees its own node — only peers
- A controller prompt instructs the agent to use tools (especially bash)
  and never guess at system details

---

## Long-running tasks and checkpoints

`swarm_dispatch` is a **synchronous** tool — it blocks the caller until
the node finishes (capped by the hub's dispatch timeout, default 600 s).
That's fine for quick commands but unusable for work measured in hours,
like fine-tuning a model, crunching a dataset, or running a batch job.

For long-running work, use the **async dispatch path** instead:

### `swarm_submit` — fire and acknowledge

```
swarm_submit { prompt, payloads?, timeout_secs?, constraints? }
  → { task_id, node_id, submitted_at_unix, state: "running" }
```

Returns in a few milliseconds.  The task is signed, pushed to the
selected node over SSE, and tracked in the hub's in-memory `TaskStore`.
No oneshot waiter is registered; the caller discovers progress and the
final result by polling.

### `swarm_checkpoint` — progress from inside the task

A built-in tool the swarm controller's agent can call during execution:

```
swarm_checkpoint { message, progress? }
```

Each call attaches a `CheckpointEvent` to the tool's `ToolOutput` as a
side-channel (the same pattern `send_file` uses for file delivery).
The swarm controller's `Output::checkpoint` hook routes the event to a
per-task mpsc channel whose forwarder task POSTs it to
`POST /swarm/checkpoint` on the hub.  The hub appends the event to the
task's record, bumping a monotonic sequence number.  Outside of a
running swarm task the default `Output::checkpoint` impl drops the
event, so the tool is a harmless no-op for terminal / telegram agents.

The agent is encouraged (via the controller prompt) to emit checkpoints
at natural milestones — once per epoch during training, once per batch
during processing, once per stage of a pipeline.

### Polling MCP tools

| Tool | Purpose |
|------|---------|
| `swarm_task_status { task_id }` | Lightweight state: `state`, `checkpoint_count`, `last_sequence`, timestamps |
| `swarm_task_checkpoints { task_id, since_sequence? }` | Ordered checkpoint list with sequence strictly greater than `since_sequence` (default 0) — tail progress incrementally |
| `swarm_task_result { task_id }` | `{ state, result? }`.  `result` is absent while running, present once terminal |
| `swarm_task_cancel { task_id }` | Mark a running task cancelled and push a `cancel_task` SSE event to the owning node |
| `swarm_task_list { limit? }` | Recent tasks newest-first, bounded by `limit` (default 50). Includes sync dispatches too — every task flows through the same store |

`swarm_task_result` uses a single shape: while running, only `state`
appears; once the task is terminal (`completed` / `failed` / `cancelled`)
the full `SwarmResult` is attached.  Callers can't confuse "still
running" with "completed successfully" because `result` is only present
in the terminal case.

### Storage and lifetime

`TaskRecord`s are kept in-memory on the hub only — there is **no
persistence across hub restart**.  The same reaper task that culls stale
nodes also drops terminal tasks older than 24 hours.  Long-running
callers should retrieve their results before the hub is restarted.

### Cancellation

`swarm_task_cancel { task_id }` marks the task `Cancelled` on the hub,
wakes any blocking `swarm_dispatch` caller with a synthetic Cancelled
result, and pushes a `cancel_task` SSE event to the owning node.  On
the node, the swarm controller drops the in-flight `agent.run` future
via `tokio::select!` against the per-task `CancellationToken`.  Tools
that poll `ctx.cancellation` (currently `web_fetch` and `web_search`)
observe the token directly; other tools — including bash — only stop
at the next `await` point when the enclosing future is dropped.

This makes cancellation **cooperative, not instant**: a bash command
already running will keep going until it completes its current output
read, then the tool result is discarded.  That's acceptable for most
long-running jobs (training loops usually yield regularly), but
you should treat cancellation as "please stop soon" rather than a
hard kill.  If the node races the hub and POSTs a real result after
cancellation, `TaskStore::finalize` preserves the `Cancelled` state
(first writer wins).

### What v1 still doesn't do

- **Persistence**: hub restart loses all in-flight and recent task state.
- **Queueing**: if no node is eligible, both `swarm_dispatch` and
  `swarm_submit` fail fast with `no eligible node`.
- **Automatic progress**: checkpoints are explicitly emitted by the
  agent calling `swarm_checkpoint`.  There's no automatic scraping of
  bash stdout.
- **Hard kill of bash subprocesses on cancel**: tools that don't check
  `ctx.cancellation` (e.g. bash) keep running until their next async
  yield point.

### Example flow — fine-tune a model

```
1. Caller:  swarm_submit { prompt: "fine-tune meta-llama/Llama-3.1-8B on data.jsonl ...",
                           constraints: { needs_gpu: true, min_ram_gb: 64 } }
            → { task_id: "abc-123", node_id: "gpu-02", state: "running" }

2. Node agent during execution:
   - loads dataset
   - calls swarm_checkpoint { message: "dataset loaded", progress: 0.05 }
   - starts training
   - per epoch: swarm_checkpoint { message: "epoch N/10 loss=...", progress: N/10 }
   - saves weights to ./model.safetensors (picked up via send_file)
   - finishes

3. Caller polls every minute:
   - swarm_task_status { task_id: "abc-123" }
     → { state: "running", checkpoint_count: 4, last_sequence: 4, ... }
   - swarm_task_checkpoints { task_id: "abc-123", since_sequence: 0 }
     → all four progress lines so far

4. Eventually: swarm_task_result { task_id: "abc-123" }
   → { state: "completed", result: { text: "...", payloads: [...] } }
```

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
              ---  -------------------------
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

## Network Security

The hub has two security layers that are enforced by default on
non-localhost interfaces.  Both must be explicitly disabled if you
want to run without them.

Binding to an external interface requires **both** `--dangerous-no-tls`
and `--dangerous-no-auth` if you don't have TLS configured:

```bash
# This will fail — two separate checks must pass:
swarm --bind 0.0.0.0:8080 --data-dir ./hub-data

# This works — both risks explicitly acknowledged:
swarm --bind 0.0.0.0:8080 --data-dir ./hub-data \
      --dangerous-no-tls --dangerous-no-auth
```

Localhost (`127.0.0.1`, `::1`) skips both checks automatically.

### TLS (transport encryption)

TLS is **mandatory** when binding to a non-localhost address.  The hub
refuses to start without it.

**Manual TLS:**

```bash
swarm --bind 0.0.0.0:443 --data-dir ./hub-data \
      --cert /path/to/fullchain.pem \
      --private-key /path/to/privkey.pem
```

**Let's Encrypt (automatic):**

```bash
swarm --bind 0.0.0.0:443 --data-dir ./hub-data \
      --letsencrypt --domain hub.example.com \
      --letsencrypt-email admin@example.com
```

Certificates are provisioned via TLS-ALPN-01 (same port, no port 80
needed) and cached in `--cert-cache-dir` (default: `.swarm-certs`).

**Disabling TLS (not recommended):**

```bash
swarm --bind 0.0.0.0:8080 --data-dir ./hub-data --dangerous-no-tls
```

### Authentication

The hub requires `--dangerous-no-auth` when binding to a non-localhost
address.  There is no pluggable auth system yet — this flag exists to
make the lack of authentication explicit and intentional.

The hub uses bearer tokens for node-level authentication.  When a node
registers via `POST /swarm/register`, the hub generates a random
32-byte token and returns it.  All subsequent requests from that node
must include `Authorization: Bearer <token>`.

Protected endpoints: `/swarm/events`, `/swarm/heartbeat`,
`/swarm/result`, `/swarm/blob`.

Unprotected endpoints: `/swarm/register` (to obtain a token), `/mcp`.

The `/mcp` endpoint is open because it serves tool calls from any
Dyson agent — not just registered nodes.  Ed25519 task signing
provides integrity for dispatched work, but the MCP endpoint itself
has no auth gate.

### Deployment patterns

**Localhost (development):**

```bash
swarm --bind 127.0.0.1:8080 --data-dir ./hub-data
```

No TLS, no flags.  Good for local testing.

**SSH port forwarding:**

```bash
# Hub machine — localhost only
swarm --bind 127.0.0.1:8080 --data-dir ./hub-data

# Each node — forward local 8080 to the hub
ssh -L 8080:127.0.0.1:8080 user@hub-host -N
```

Nodes point at `http://127.0.0.1:8080` — traffic is encrypted via SSH.

**Tailscale / WireGuard:**

```bash
swarm --bind 100.x.y.z:443 --data-dir ./hub-data \
      --cert cert.pem --private-key key.pem
```

All traffic stays within the mesh.

**Public internet:**

```bash
swarm --bind 0.0.0.0:443 --data-dir ./hub-data \
      --letsencrypt --domain hub.example.com
```

TLS is mandatory.  Consider additionally restricting access at the
firewall level to known node IPs.

---

## Testing

```bash
# All swarm tests (28 tests)
cargo test --lib -p dyson swarm

# Controller tests (9 tests)
cargo test --lib -p dyson controller::swarm
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
