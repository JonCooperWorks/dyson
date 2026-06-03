# Tool Forwarding over MCP

When Dyson uses a CLI provider as its LLM backend (`claude-code` or `codex`),
there's a fundamental challenge: the subprocess runs its own agent loop with its
own tools, and Dyson must not re-execute tool events it merely observes.

Tool forwarding over MCP solves the structured-tool side by making Dyson an MCP
server. Dyson starts a loopback HTTP server that exposes the loaded Dyson tools,
then passes the connection config to the CLI provider. Claude Code uses
`--mcp-config`; Codex receives equivalent `mcp_servers.*` `-c` settings. The
subprocess connects back, discovers the tools, and uses them natively in its
own agent loop.

**Key files:**
- `src/skill/mcp/serve/mod.rs` — HTTP MCP server (Dyson as MCP server)
- `src/llm/claude_code.rs` — Client that starts the server + spawns Claude Code
- `src/llm/codex.rs` — Client that starts the server + spawns Codex
- `src/skill/mcp/protocol.rs` — Shared JSON-RPC types (client + server)
- `src/skill/mcp/mod.rs` — Module that also contains the MCP client
- `src/llm/mod.rs` — `create_client()` factory that wires everything together

---

## The Problem

Dyson has a unified `workspace` tool that gives the agent identity and memory:

| Tool | Purpose |
|------|---------|
| `workspace` | View, list, search, or update workspace files (SOUL.md, MEMORY.md, journals, etc.) via the `op` parameter. |

With the Anthropic or OpenAI backends, this tool goes through Dyson's own agent
loop — the LLM emits `tool_use` blocks, Dyson executes them, and sends back
`tool_result`.  Simple.

But with CLI backends, the subprocess **is** the agent. Dyson streams its output
in `ToolMode::Observe` and does not run its returned tool calls again. Without
MCP, the subprocess would have no structured access to Dyson's workspace,
memory, KB, MCP-wrapped, or controller-facing tools.

---

## The Solution: MCP Server

MCP (Model Context Protocol) is a JSON-RPC 2.0 protocol that CLI providers can
use for structured tools. Dyson exploits this by becoming an MCP server:

```
┌──────────────────────────────────────────────────────────────┐
│ Dyson process                                                │
│                                                              │
│  ┌─────────────────────┐     ┌──────────────────────────┐   │
│  │ CLI LLM client      │     │ McpHttpServer            │   │
│  │                     │     │ (tokio task)             │   │
│  │ 1. Starts server ───┼────▶│ 127.0.0.1:{random_port} │   │
│  │ 2. Spawns CLI with  │     │                          │   │
│  │    MCP config       │     │ POST /mcp                │   │
│  │    '{"mcpServers":  │     │   ├─ initialize          │   │
│  │     {"dyson-        │     │   ├─ notifications/      │   │
│  │      workspace":    │     │   │  initialized         │   │
│  │      {"type":"url", │     │   ├─ tools/list          │   │
│  │       "url":        │     │   │  → loaded tools      │   │
│  │       "http://...   │     │   │                      │   │
│  │       /mcp"}}}'     │     │   │                      │   │
│  │                     │     │   └─ tools/call          │   │
│  └──────┬──────────────┘     │      → runs Tool impl   │   │
│         │ stdin/stdout        └─────────────┬────────────┘   │
│         ▼                                   │               │
│  ┌──────────────┐                 ┌─────────▼───────┐       │
│  │ cli process  │───HTTP/MCP────▶│ Arc<RwLock<     │       │
│  │ subprocess   │                 │   Box<dyn       │       │
│  │              │◀───responses────│   Workspace>>>  │       │
│  └──────────────┘                 └─────────────────┘       │
└──────────────────────────────────────────────────────────────┘
```

This is transparent when a user configures `provider: "claude-code"` or
`provider: "codex"` and a workspace is available. The MCP server starts
automatically for the duration of each provider turn and exposes the same
loaded tool registry the agent would use for API providers.

---

## How It Works

### Startup Sequence

Each time `ClaudeCodeClient::stream()` or `CodexClient::stream()` is called
(once per LLM turn):

1. **Start MCP server**: `McpHttpServer::start()` binds to `127.0.0.1:0`
   (loopback-only, OS-assigned port), generates a bearer token, and spawns a
   tokio task for the accept loop.  Returns `(port, handle, token)`.

2. **Build config JSON**: Construct the MCP server config that tells Claude Code
   where to connect and how to authenticate:
   ```json
   {
     "mcpServers": {
       "dyson-workspace": {
         "type": "url",
         "url": "http://127.0.0.1:54321/mcp",
         "headers": {
           "Authorization": "Bearer <token>"
         }
       }
     }
   }
   ```

3. **Pass to the CLI provider**: Claude Code receives the config as
   `--mcp-config`; Codex receives the same URL/token as `mcp_servers.*` config
   values.

4. **The subprocess connects**: During startup, it reads the MCP config,
   connects to Dyson's HTTP server, and runs the MCP handshake.

5. **Tools available**: The subprocess now has Dyson tools as first-class
   structured tools with proper JSON schemas.

### MCP Handshake (Server Perspective)

```
CLI provider                   McpHttpServer
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "initialize", ...}
    │◀─ 200 OK ───────────────────│  {"result": {"protocolVersion": "2024-11-05", ...}}
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "notifications/initialized"}
    │◀─ 200 OK ───────────────────│  {"result": {}}
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "tools/list"}
    │◀─ 200 OK ───────────────────│  {"result": {"tools": [workspace, read_file, ...]}}
    │                              │
    │   ... during agent loop ...  │
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "tools/call", "params": {"name": "workspace", "arguments": {"op": "view", ...}}}
    │◀─ 200 OK ───────────────────│  {"result": {"content": [{"type": "text", "text": "..."}]}}
    │                              │
```

### Tool Execution

When the CLI provider calls a Dyson tool:

1. The request arrives as a JSON-RPC `tools/call` to `POST /mcp`
2. `McpHttpServer` looks up the tool by name in its HashMap
3. Builds a `ToolContext` with the shared workspace, cwd, env, and cancellation token
4. Calls `tool.run(arguments, &ctx)` — the **same** `Tool` trait implementation
   used everywhere in Dyson
5. Wraps the `ToolOutput` in MCP content blocks and returns

The tool is not duplicated. `McpHttpServer` calls the exact same `Tool`
implementations that Dyson's own agent loop would use with API providers.

### Lifecycle

A new MCP server is created per LLM turn and cleaned up when the stream drops. Per-turn servers simplify lifecycle: no shutdown coordination, no stale connections, no port leaks.

---

## Security

- **Loopback-only** — binds to `127.0.0.1`, no network exposure
- **Bearer token** — 64-char hex from CSPRNG, regenerated per LLM turn, zeroized on drop. Requests without valid token get HTTP 401
- **Not in shell history** — subprocess spawned programmatically; token visible in `ps` but ephemeral and loopback-only
- **Defense in depth** — loopback + bearer auth + ephemeral port ensures only the intended subprocess connects

---

## Sandbox Plumbing

The `dangerous_no_sandbox` flag still controls Dyson's own sandbox posture. For
CLI providers, tool calls observed in the provider stream are not re-executed by
Dyson; structured calls that come back through the MCP server execute the shared
Dyson `Tool` implementations with their normal `ToolContext`.

---

## Two Directions of MCP

Dyson uses MCP in both directions.  This can be confusing, so here's the
distinction:

### Dyson as MCP Client (mod.rs, transport.rs)

Dyson **connects to** external MCP servers (GitHub, filesystem tools, etc.),
discovers their tools, and wraps each as an `Arc<dyn Tool>` for its agent loop.

```
Dyson agent loop → McpRemoteTool.run() → StdioTransport → external MCP server
```

Configured via `mcp_servers` in `dyson.json`.  Used with all LLM backends.

### Dyson as MCP Server (serve/mod.rs)

Dyson **serves** loaded Dyson tools to CLI providers via an HTTP MCP server.

```
CLI provider agent loop → HTTP → McpHttpServer → Tool.run() → workspace and loaded tools
```

Automatic when a CLI provider has a workspace configured. Anthropic/OpenAI-style
API backends use Dyson's own tool execution instead.

### Both at once

Both directions can be active simultaneously.  For example, with a Claude Code
backend and MCP servers configured:

- Dyson connects to the GitHub MCP server as a client (McpSkill)
- Dyson serves loaded Dyson tools to Claude Code and Codex as a server (McpHttpServer)
- Claude Code or Codex has its own native tools plus Dyson-loaded tools exposed
  through the loopback MCP server, including external MCP tools that Dyson
  wrapped as `McpRemoteTool`

---

## Bidirectional MCP (full spec surface)

Beyond `tools/*`, Dyson implements the rest of the MCP primitives in both
directions.  The load-bearing change is that the transports
(`transport.rs`) are **bidirectional**: after `initialize`, a server may
originate its own JSON-RPC *requests* and *notifications*, and the client
answers them.  The background reader on each transport classifies every
inbound line (response / request / notification) and routes server-
originated traffic to an `InboundHandler` — the `NotificationRouter`
(`router.rs`) the skill installs per connection.

### Capability negotiation

`initialize` is where each side declares what it supports, and every
feature gates on the negotiated set so we never round-trip a `-32601`:

- **Client → server** advertises `roots` always, `sampling` when the agent
  supplied LLM context (it does outside the headless probe), and
  `elicitation` only when a UI is present (set by the HTTP controller at
  startup — a headless run must not strand a server on a prompt nobody can
  answer).
- **Server → client** (`serve/mod.rs`) advertises `tools`, `resources`,
  `prompts`, and `completions`.

### Server-originated requests the client answers (`router.rs`)

| Method | Handler |
|--------|---------|
| `roots/list` | Returns the agent's working directory as the one filesystem root. |
| `sampling/createMessage` | Translates the MCP messages into a dyson completion, runs a one-shot `LlmClient` (via `create_client`, mirroring the per-session pattern), and returns the assistant text. |
| `elicitation/create` | Parks the prompt in a process-global broker (`elicitation.rs`); the web UI short-polls `GET /api/mcp/elicitations` and answers via `POST /api/mcp/elicitations/:id`. |

Server-originated **notifications** (`notifications/message` logging,
`progress`, `cancelled`, the `*/list_changed` family) are routed through
`tracing`.

### Outbound primitives the client can call

When a connected server advertises the capability, the skill registers a
tool so the agent can use it: `<server>_resources` (`resources/list` +
`resources/read`, bytes saved through `save_mcp_resource`) and
`<server>_prompts` (`prompts/list` + `prompts/get`).  The client also
opts into logs with `logging/setLevel` during the handshake.

### Server-side primitives Dyson exposes (`serve/mod.rs`)

`resources/list` + `resources/read` expose workspace files under the
`workspace://` scheme; `prompts/list` + `prompts/get` expose workspace
skills (`skills/<name>/SKILL.md`); `completion/complete` backs resource-URI
autocompletion.  This per-turn POST→response transport has **no channel to
push** server-originated messages, so `logging` and the `*/list_changed`
notifications are deliberately not advertised there.

---

## Error Handling

The MCP server uses standard JSON-RPC 2.0 error codes for protocol errors and
MCP's `isError` field for tool-level errors:

| Scenario | Response |
|----------|----------|
| Missing or invalid bearer token | HTTP 401, `{"error": "unauthorized"}` |
| Invalid JSON body | HTTP 400, JSON-RPC error -32700 (Parse error) |
| Unknown method | HTTP 200, JSON-RPC error -32601 (Method not found) |
| Missing params | HTTP 200, JSON-RPC error -32602 (Invalid params) |
| Unknown tool name | HTTP 200, MCP result with `isError: true` |
| Tool execution fails | HTTP 200, MCP result with `isError: true` |
| Non-POST or wrong path | HTTP 404 |

This matches the MCP specification's error model: protocol errors use JSON-RPC
error codes; tool errors use the `isError` field in the result body.

---

## Testing

The MCP server has both unit and integration tests:

- **Unit tests** call `server.dispatch()` directly, testing JSON-RPC routing,
  parameter validation, tool execution, and error handling without HTTP.

- **Integration test** starts the real HTTP server, sends a request via
  `reqwest`, and validates the full stack from TCP accept to JSON response.

- **Auth tests** (`tests/security_regression.rs`) verify that:
  - Requests without an `Authorization` header are rejected with 401
  - Requests with a wrong bearer token are rejected with 401
  - Requests with the correct bearer token succeed

- **MockWorkspace** provides a minimal in-memory workspace implementation
  with one file for verifying tool execution end-to-end.

Run with:
```bash
cargo test skill::mcp::serve           # unit tests
cargo test mcp_server                   # security regression tests
```
