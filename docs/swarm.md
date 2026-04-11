# Swarm

The swarm controller enables dynamic resource routing across Dyson nodes.
Adding a swarm controller makes this Dyson both a **worker** (the hub can
send it tasks) and a **client** (its agent can dispatch tasks to other nodes).
Participation is symmetric: you use the swarm because you are part of it.

> **Long-running tasks** — the swarm now supports the submit-and-poll
> model. Use `swarm_submit` for fire-and-forget tasks (overnight
> autoresearch, fine-tuning), get a Telegram or webhook ping when each
> one finishes, and resume your agent the next morning with
> `swarm_results` to catch up. See **[Long-running tasks](#long-running-tasks)**
> below.

**Key files (worker side, `crates/dyson/`):**
- `src/swarm/connection.rs` — `SwarmConnection` (SSE inbound, POST outbound,
  per-task progress heartbeats)
- `src/swarm/probe.rs` — `HardwareProbe` (GPU/CPU/RAM/disk detection)
- `src/controller/swarm.rs` — `SwarmController` with cooperative
  cancellation via `tokio_util::sync::CancellationToken`
- `src/config/mod.rs` — `SwarmControllerConfig`
- `src/command/listen.rs` — Controller factory + MCP auto-wiring

**Key files (hub side, `crates/swarm/`):**
- `src/lib.rs` — `Hub` shared state: registry + blob store + scheduler
  task store + notifier
- `src/registry.rs` — In-memory `NodeRegistry` (ephemeral, dies with
  the hub process)
- `src/router.rs` — `select_node()` constraint matcher
- `src/key.rs` — `HubKeyPair`, V1 Ed25519 task signing
- `src/scheduler/store.rs` — **SQLite-backed** task table; survives
  hub restart
- `src/scheduler/types.rs` — `TaskState`, `NotifyChannel`,
  `ProgressReport`, `TerminalStatus`
- `src/notifier/` — Background worker that fires telegram / webhook /
  stdout notifications when tasks reach terminal state
- `src/http/mcp.rs` — MCP tool surface
  (`swarm_submit` / `swarm_status` / `swarm_logs` / `swarm_cancel` /
  `swarm_results` / `swarm_await` / `swarm_dispatch`)
- `src/http/progress.rs` — `POST /swarm/task/{id}/progress` per-task
  heartbeat endpoint
- `src/http/result.rs` — Wires terminal results into both the legacy
  oneshot caller path and the durable task store + notifier

**Key files (mesh primitives, `crates/dyson-mesh/`):**
- `src/addr.rs` — `NodeId`, `ServiceName`, `MeshAddr`
- `src/identity.rs` — `NodeIdentity` (Ed25519 keypair persisted to
  `~/.dyson/node.key`)
- `src/envelope.rs` — `MeshEnvelope`, `MessageKind`, `RequestId`
  (UUIDv7), end-to-end signed
- `src/mesh.rs` — `MeshClient` trait
- `src/inproc.rs` — `InProcMeshClient` (in-memory channels for tests
  and hub-local services)
- `src/mailbox.rs` — Per-peer TTL mailbox for disconnect tolerance

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

## Long-running tasks

The swarm is designed for two very different workloads, both running on the
same infrastructure:

1. **Short interactive dispatch.** Sub-minute tasks an agent can block on:
   "summarise this PDF on the GPU node", "run this bash command on the
   linux box". The legacy `swarm_dispatch` tool handles these — it signs
   the task, pushes it to a worker, and blocks the MCP caller on a
   oneshot for up to 600 seconds.

2. **Long-running fire-and-forget.** Multi-hour or overnight jobs:
   "fine-tune this model overnight", "run autoresearch for 8 hours
   exploring hyperparameter space". Blocking on a oneshot is wrong here
   — the agent dispatching the task needs to continue helping the user
   with full tools, the hub or the agent might restart, and the user
   wants a Telegram ping when it's done so they can resume.

The submit/poll surface handles case 2:

```text
[user, mid-conversation with full-tool agent]
  "fine-tune llama-3 on this dataset overnight, ping me on tg when done"

[agent calls swarm_submit via MCP, gets task_id back in ~50 ms]
  "submitted task t_abc123, i'll let you know. what's next?"

[user keeps working with full tools for hours]

[overnight: GPU node runs autoresearch loop, checkpoints to blob store,
 sends per-task progress heartbeats every 30 seconds]

[task completes, hub fires the configured Telegram notification]
  "swarm task t_abc123 done (autoresearch-finetune): eval_loss=0.0312..."

[next morning: user taps the message, agent calls swarm_results(since=...)
 to pull the full state, resumes with: 'your fine-tune finished at 3am,
 best run was experiment 7, want me to eval it against the holdout set?']
```

The design rules out a few things explicitly:

- **Not a real mesh.** This is hub-and-spoke. There is one coordinator
  and a set of workers. No gossip, no peer discovery, no consensus, no
  Byzantine tolerance. The `dyson-mesh` crate gives the codebase the
  shape it would need to add a future P2P transport behind a trait, but
  the deployed topology is a single hub. The hub is a SPOF; document
  that to operators.
- **No broadcast.** `swarm_submit` places one job on one worker. If a
  caller wants N parallel jobs, they submit N tasks.
- **Cancellation = checkpoint and exit.** Cancellation is *cooperative*
  on the worker side (a `tokio_util::sync::CancellationToken` is
  threaded into the agent execution future). Skills running long
  experiments are responsible for noticing cancellation, uploading the
  current best checkpoint as a blob, and returning a partial result.
  The current default is "drop the future at next await point and
  report `TaskStatus::Cancelled`"; richer skills can layer on top.
- **At-least-once, never exactly-once.** The relay (and the hub HTTP
  layer it's grown out of) is allowed to lose messages. Services
  recover via retries and idempotency on `request_id` (UUIDv7). The
  scheduler's SQLite task table is the durable source of truth.

### Lifecycle states

```text
Pending  ─────►  Assigned  ─────►  Running ◄──────► Stalled
                                      │
                                      ├──────► Done       (terminal)
                                      ├──────► Failed     (terminal)
                                      ├──────► Cancelling ──► Cancelled (terminal)
```

| State | Means | Transition rules |
|-------|-------|------------------|
| `pending` | Submitted but not yet dispatched | → `assigned` when a worker is picked |
| `assigned` | Sent to a worker over SSE; waiting for first heartbeat | → `running` on first progress report |
| `running` | Worker reported progress recently | → `stalled` if no progress within the per-skill threshold; → terminal on result |
| `stalled` | Hub hasn't seen progress for too long | Not auto-reassigned. Slides back to `running` on next progress report. |
| `cancelling` | `swarm_cancel` was called; waiting for the worker to checkpoint and exit | → `cancelled` on result |
| `done` / `failed` / `cancelled` | Terminal | Notifier fires once, `notification_delivered` flag set on success |

### Durable state

The hub stores tasks in `data_dir/tasks.sqlite` (WAL mode). Restart-safe:

- a hub restart loses the in-memory `pending_dispatches` (legacy
  `swarm_dispatch` callers will time out) but **does not** lose
  scheduler tasks
- workers reconnect via the existing reconnect-with-backoff loop and
  resume reporting progress
- the next `swarm_results` call surfaces everything that finished while
  the hub or the agent was offline

### Notifications

A `swarm_submit` request can attach one or more notification channels
that fire when the task reaches a terminal state. Three transports today:

- `stdout` — prints to the hub's tracing log; useful for local dev
- `webhook` — `POST` JSON to a URL
- `telegram` — `POST` to `https://api.telegram.org/bot{token}/sendMessage`

Templates use `{{var}}` substitution only — no Handlebars, no Jinja.
Available variables: `task_id`, `state`, `skill`, `summary`, `error`,
`duration_secs`, `assigned_node`. Default template:

```text
swarm task {{task_id}} {{state}} ({{skill}}): {{summary}}
```

Delivery is retried with exponential backoff for ~1 hour total. On
final failure, the task row is left with `notification_delivered = 0`
so the next `swarm_results` call surfaces "task done, notification
never delivered" and the user can ask the agent to redeliver manually.

### Cancellation

Cancellation is initiated from MCP via `swarm_cancel({"task_id": ...})`.
Wire flow:

1. MCP handler transitions the task row to `cancelling` and pushes a
   `Cancel { task_id }` SSE event to the assigned worker.
2. The worker's controller event loop sees the event mid-execution
   (the loop is structured around `tokio::select!` so SSE events flow
   even while a task is running) and trips the
   `tokio_util::sync::CancellationToken` for that task.
3. The agent execution future is dropped at its next await point. The
   controller assembles a `SwarmResult { status: Cancelled }` from
   whatever output was captured and POSTs it back.
4. The hub's `result_handler` finishes the task row in the scheduler
   store and fires the notifier.

A cancelled task is final. There is no resume — the next run starts
fresh. Skills that need to checkpoint partial work upload it as a blob
in their cancellation handler before returning.

### Per-task progress

While a task runs, the worker sends a progress heartbeat every 30
seconds via `POST /swarm/task/{task_id}/progress`. The body is a
`ProgressReport { task_id, progress, message, log }` — all fields
optional. The hub:

- updates `last_progress_at_ms` (slides the task from `stalled` back
  to `running` if needed)
- updates `progress_pct` and `progress_message` (visible in
  `swarm_status({"task_id": ...})`)
- appends any `log` chunk to the per-task log (visible in
  `swarm_logs`)

A separate stall-sweep task runs in the hub and slides any task whose
last progress is older than the per-skill threshold to `stalled`. The
stall sweep does not auto-reassign — long-running tasks usually can't
be safely re-run from the start.

### MCP tools

The full MCP surface today:

| Tool | Blocking? | Purpose |
|------|-----------|---------|
| `list_nodes` | no | Enumerate registered workers (filtered by `?caller=`) |
| `swarm_status` | no | Aggregate counts; pass `{"task_id": "..."}` for one task |
| `swarm_dispatch` | yes (≤ 600s) | DEPRECATED legacy synchronous dispatch — use `swarm_submit` + `swarm_await` |
| `swarm_submit` | no | Submit a task, return its `task_id` immediately. Body: `prompt, skill, payloads, timeout_secs, constraints, notify` |
| `swarm_await` | yes (configurable) | Block on a `task_id` until the deadline elapses |
| `swarm_logs` | no | Tail captured logs / progress messages for a task |
| `swarm_cancel` | no | Request cancellation; worker checkpoints and exits |
| `swarm_results` | no | List recent terminal tasks. Use this on agent startup to catch up on what happened while you were gone. |

`swarm_submit` example body:

```json
{
  "prompt": "fine-tune llama-3 on this dataset and report eval loss",
  "skill": "autoresearch-finetune",
  "payloads": [{ "type": "ref", "name": "dataset.parquet", "sha256": "...", "size": 1073741824 }],
  "timeout_secs": 28800,
  "constraints": { "needs_gpu": true, "min_ram_gb": 64 },
  "notify": [
    {
      "kind": "telegram",
      "bot_token": "12345:abc",
      "chat_id": "67890",
      "template": "✅ {{skill}} done: {{summary}} (took {{duration_secs}}s)"
    }
  ]
}
```

### Reachback for agents

When an agent dispatches a long-running task and then exits its
session, there's no live process for the notifier to call back into.
Instead the workflow is:

1. The user (or an `stdout` notification line in the hub log) tells
   the agent something finished.
2. On startup, agents call `swarm_results(since=last_seen_ms)` to
   catch up on terminal tasks.
3. They can then call `swarm_status({"task_id": ...})` /
   `swarm_logs({"task_id": ...})` to dive into specific tasks before
   responding to the user.

This pattern requires no special "callback" infrastructure. The
durable task store is the reachback mechanism.

---

## Mesh primitives (forward-compat layer)

The `dyson-mesh` crate (`crates/dyson-mesh/`) defines the abstractions
that let nodes talk to each other without caring about the physical
topology. It is a deliberate **structural foundation** — the deployed
topology is still hub-and-spoke, and there is no gossip / discovery /
consensus impl in this codebase. What it gives you is the right shape:
the scheduler, notifier, and worker code can evolve toward a peer
abstraction without rewriting their wire transport later.

| Type | Lives in | Purpose |
|------|----------|---------|
| `NodeId` | `addr.rs` | Self-authenticating peer identity (base64url of an Ed25519 public key, 43 chars) |
| `ServiceName` | `addr.rs` | The name of a service hosted on a peer (`scheduler`, `notifier`, `mcp`, …) |
| `MeshAddr` | `addr.rs` | Fully qualified `node_id/service` address |
| `NodeIdentity` | `identity.rs` | Persistent Ed25519 keypair at `~/.dyson/node.key` (mode `0600`); delete the file = become a new peer |
| `MeshEnvelope` | `envelope.rs` | The wire envelope: `version, from, to, request_id, correlation_id, ts_ms, ttl_secs, kind, body, signature`. End-to-end Ed25519 signed. |
| `MessageKind` | `envelope.rs` | Typed discriminant: `SubmitTask, TaskAssign, TaskAccepted, TaskProgress, TaskResult, TaskCancel, TaskCancelAck, RegisterNotification, McpCall, McpReply, Custom { name }` |
| `RequestId` | `envelope.rs` | UUIDv7 — sortable, timestamp-prefixed, doubles as an idempotency key |
| `MeshClient` | `mesh.rs` | The trait every transport implements: `announce, depart, peers, peer_events, send, inbox, fetch_blob, publish_blob` (the last two are future work) |
| `InProcMeshClient` | `inproc.rs` | In-memory channel impl. Used by the `dyson-mesh` test suite to spin up multi-peer scenarios in a single process; in production it short-circuits hub-local services that share a process with the relay. |
| `Mailbox` | `mailbox.rs` | Per-peer FIFO queue with per-envelope TTL. Default 10 min, hard cap 1 h. |

### Honest framing

> **This is not a real mesh.** Today the hub is hand-wired into the
> existing `/swarm/*` HTTP endpoints. The `dyson-mesh` types are *the
> shape we're growing into*, not the shape that's deployed. A future
> `HttpMeshClient` can wrap the existing transport, and a future
> `GossipMeshClient` can drop in behind the same trait without
> rewriting the scheduler / notifier / worker. None of that exists
> yet, and the hub remains a single point of failure.

The reason the abstraction is checked in now, before it's wired up,
is that two pieces of design clarity are load-bearing:

1. **Identity and addressing.** Once `NodeId = base64url(pubkey)` is
   in the protocol, every signed message is self-authenticating and
   the bearer-token machinery becomes optional. Adding this later is
   a flag-day migration.
2. **Envelope shape.** The `MeshEnvelope` carries `request_id`,
   `correlation_id`, `ttl`, and `kind` from day one. Services that
   migrate to consume an inbox of envelopes (rather than typed HTTP
   handlers) get retries, idempotency, and observability for free.

---

## Architecture

```
+--------------------------------------------------------+
|                     SWARM HUB                          |
|                                                        |
|  /mcp             <- MCP server (tool discovery)       |
|  /swarm/register  <- POST (node registration)          |
|  /swarm/events    <- SSE (push tasks to nodes)         |
|  /swarm/heartbeat <- POST (node status updates)        |
|  /swarm/result    <- POST (task results from nodes)    |
|  /swarm/blob/{sha256} <- GET/PUT (large payloads)      |
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
| `/mcp` | POST | MCP JSON-RPC — tool discovery + the full long-running task surface |
| `/swarm/register` | POST | Node registration, returns `{ node_id, token }` |
| `/swarm/events` | GET | SSE stream for pushing tasks (and cancellations) to a node |
| `/swarm/heartbeat` | POST | Node-level heartbeat (worker is alive at all) |
| `/swarm/task/{task_id}/progress` | POST | **Per-task** heartbeat: progress fraction, message, log chunk |
| `/swarm/result` | POST | Final result from a worker. Wakes any synchronous `swarm_dispatch` caller AND writes the terminal state to the SQLite task store + fires the notifier. |
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
| `cancel` | `{ "task_id": "..." }` | Hub requests cancellation of an in-flight task |
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
        |     |-- Spawn per-task progress heartbeat (every 30s)
        |     |-- tokio::select! over:
        |     |     - agent.run(prompt) (with optional timeout)
        |     |     - cancellation token (trips on SSE "cancel" event)
        |     |     - inner SSE recv (so cancel arrives mid-task)
        |     |-- POST /swarm/result (Completed | Failed | Cancelled)
        |     +-- agent.clear() (reset for next task)
        |
        |-- SSE "cancel" (while idle) -> debug log, no-op
        |-- SSE "shutdown" -> break
        +-- SSE disconnect -> reconnect with exponential backoff
              (base 2s, capped at 60s, max 10 attempts)
```

The event loop is structured around `tokio::select!` so cancel events
flow even while a task is running. Cancellation is *cooperative*:
dropping the agent.run() future at the next await point is enough to
stop the task. The captured `output.text()` is reported as the
cancelled result body so the agent's partial work is preserved.

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
the hub's MCP tools — `swarm_dispatch`, `swarm_status`, `list_nodes`.

The swarm controller's own agent is special:
- `swarm_dispatch` is excluded (prevents recursive task loops)
- `list_nodes` results are filtered by the hub (via `?caller=`) so the
  agent never sees its own node — only peers
- A controller prompt instructs the agent to use tools (especially bash)
  and never guess at system details

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
# Mesh primitives (28 tests — addr, identity, envelope, mailbox, inproc client)
cargo test -p dyson-mesh

# Swarm hub: registry, router, blob store, key signing, MCP parsing,
# scheduler store, notifier templates, integration tests
cargo test -p swarm

# Worker side: SwarmConnection SSE parser, hardware probe, controller config
cargo test --lib -p dyson swarm
cargo test --lib -p dyson controller::swarm
```

The scheduler and notifier add ~12 unit tests covering the SQLite task
lifecycle (submit → assign → progress → finish, stall sweep, cancel
transitions, recent_results ordering, notification delivery flag) and
the `{{var}}` template renderer. The mesh crate adds 28 tests covering
the envelope sign/verify roundtrip, mailbox TTL semantics, and the
in-process client's mailbox-on-attach drain.

Tests cover:
- Ed25519 sign/verify roundtrip, tampered payload, tampered signature, wrong key, wrong version
- Public key parsing: valid, invalid version, bad base64, wrong length
- SSE event parsing: registered, task, heartbeat_ack, cancel, shutdown, unknown, incomplete, multiline
- nvidia-smi output parsing: single GPU, multiple GPUs, empty, malformed
- `/proc/cpuinfo` parsing: single model, mixed models, empty (Linux only)
- `/proc/meminfo` parsing: normal, missing, empty (Linux only)
- macOS VRAM string parsing, system_profiler JSON parsing (macOS only)
- SwarmControllerConfig: valid, defaults, missing required fields
- Inline payload fetch and verification
- SQLite task lifecycle: submit → assign → progress → finish, with state-transition error cases
- Stall sweep moving idle running tasks to `stalled`
- `recent_results` ordering by submitted_at_ms desc
- Notification delivery flag flipped on stdout-channel completion
- `{{var}}` template renderer: substitution, missing keys, unterminated placeholders
- Mesh primitives: NodeIdentity persistence + roundtrip, signature reject paths, envelope sign/verify, mailbox TTL drain, InProcMeshClient send and mailbox-on-attach

---

## What is NOT yet built

Honest scope. The plan is bigger than what's deployed.

- **No real P2P transport.** The `dyson-mesh` crate is a structural
  foundation only. There is no `HttpMeshClient` or `GossipMeshClient`
  yet — services still talk over the existing `/swarm/*` HTTP
  endpoints. The hub remains a SPOF.
- **No reference long-running skill.** The autoresearch-finetune skill
  that motivates the submit/poll surface needs a GPU node to test
  against and is not in this branch. The infrastructure (durable
  tasks, cancellation, progress, notifications) is built so the skill
  drops in cleanly.
- **No relay refactor.** The hub's HTTP handlers still know about
  task-specific shapes. A future refactor will collapse them behind a
  single `MeshClient` inbox + `MeshEnvelope` body and split the
  scheduler / notifier / MCP services into mesh services that just
  happen to be hosted on the hub peer.
- **`InProcMeshClient` is not yet wired into the hub.** Today the hub
  uses direct method calls into the scheduler / notifier from the HTTP
  handlers. Once the relay refactor lands, hub-local services will
  connect to themselves via `InProcMeshClient` for the in-process
  short-circuit.
- **Bearer auth still in place.** The plan is to delete `auth.rs` once
  every peer (worker and hub) has a `NodeIdentity` and the relay
  authenticates via challenge-response. Until then, bearer tokens
  remain the auth gate for `/swarm/heartbeat`, `/swarm/result`,
  `/swarm/task/{id}/progress`, and `/swarm/blob`.
- **No streaming blob upload/download.** The blob store is in-memory
  per request. For multi-GB checkpoints from autoresearch-finetune,
  we'll need streaming bodies and a retention sweep.
- **No HA / consensus / leader election.** Out of scope for v1.
