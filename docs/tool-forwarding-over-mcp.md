# Tool Forwarding over MCP

When Dyson uses the Claude Code CLI as its LLM backend (`provider: "claude_code"`),
there's a fundamental challenge: Claude Code runs its own agent loop with its own
tools (Bash, Read, Write, Edit, etc.), and Dyson can't inject custom tools into
that loop directly.

Tool forwarding over MCP solves this by making Dyson an MCP server.  Dyson starts
an HTTP server that exposes workspace tools, and passes the connection config to
Claude Code via `--mcp-config`.  Claude Code connects back, discovers the tools,
and uses them natively in its agent loop — all transparently, with no special
configuration needed.

**Key files:**
- `src/skill/mcp/serve.rs` — HTTP MCP server (Dyson as MCP server)
- `src/llm/claude_code.rs` — Client that starts the server + spawns Claude Code
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

But with the Claude Code backend, Claude Code **is** the agent.  Dyson just
streams its output.  Claude Code has no concept of Dyson's workspace tool.
We can't shove it into Claude Code's tool list — it manages its own tools
internally.

---

## The Solution: MCP Server

MCP (Model Context Protocol) is a JSON-RPC 2.0 protocol that Claude Code natively
supports for extending its tool set.  Dyson exploits this by becoming an MCP server:

```
┌──────────────────────────────────────────────────────────────┐
│ Dyson process                                                │
│                                                              │
│  ┌─────────────────────┐     ┌──────────────────────────┐   │
│  │ ClaudeCodeClient    │     │ McpHttpServer            │   │
│  │                     │     │ (tokio task)             │   │
│  │ 1. Starts server ───┼────▶│ 127.0.0.1:{random_port} │   │
│  │ 2. Spawns claude -p │     │                          │   │
│  │    --mcp-config     │     │ POST /mcp                │   │
│  │    '{"mcpServers":  │     │   ├─ initialize          │   │
│  │     {"dyson-        │     │   ├─ notifications/      │   │
│  │      workspace":    │     │   │  initialized         │   │
│  │      {"type":"url", │     │   ├─ tools/list          │   │
│  │       "url":        │     │   │  → workspace         │   │
│  │       "http://...   │     │   │                      │   │
│  │       /mcp"}}}'     │     │   │                      │   │
│  │                     │     │   └─ tools/call          │   │
│  └──────┬──────────────┘     │      → runs Tool impl   │   │
│         │ stdin/stdout        └─────────────┬────────────┘   │
│         ▼                                   │               │
│  ┌──────────────┐                 ┌─────────▼───────┐       │
│  │ claude -p    │───HTTP/MCP────▶│ Arc<RwLock<     │       │
│  │ subprocess   │                 │   Box<dyn       │       │
│  │              │◀───responses────│   Workspace>>>  │       │
│  └──────────────┘                 └─────────────────┘       │
└──────────────────────────────────────────────────────────────┘
```

This is completely transparent — when a user configures `provider: "claude_code"`
and has a workspace, the MCP server starts automatically.  No extra config needed.

---

## How It Works

### Startup Sequence

Each time `ClaudeCodeClient::stream()` is called (once per LLM turn):

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

3. **Pass to Claude Code**: The config (including the bearer token) is passed as
   a CLI argument via `--mcp-config`.

4. **Claude Code connects**: During startup, Claude Code reads the MCP config,
   connects to our HTTP server, and runs the MCP handshake.

5. **Tools available**: Claude Code now has the unified `workspace` tool as a
   first-class structured tool with a proper JSON schema.  The LLM can call
   it just like Bash, Read, or Write.

### MCP Handshake (Server Perspective)

```
Claude Code                    McpHttpServer
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "initialize", ...}
    │◀─ 200 OK ───────────────────│  {"result": {"protocolVersion": "2024-11-05", ...}}
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "notifications/initialized"}
    │◀─ 200 OK ───────────────────│  {"result": {}}
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "tools/list"}
    │◀─ 200 OK ───────────────────│  {"result": {"tools": [workspace]}}
    │                              │
    │   ... during agent loop ...  │
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "tools/call", "params": {"name": "workspace", "arguments": {"op": "view", ...}}}
    │◀─ 200 OK ───────────────────│  {"result": {"content": [{"type": "text", "text": "..."}]}}
    │                              │
```

### Tool Execution

When Claude Code calls a workspace tool:

1. The request arrives as a JSON-RPC `tools/call` to `POST /mcp`
2. `McpHttpServer` looks up the tool by name in its HashMap
3. Builds a `ToolContext` with the shared `Arc<RwLock<Box<dyn Workspace>>>`
4. Calls `tool.run(arguments, &ctx)` — the **same** `Tool` trait implementation
   used everywhere in Dyson
5. Wraps the `ToolOutput` in MCP content blocks and returns

The tool is not duplicated.  `McpHttpServer` uses the exact same
`WorkspaceTool` that Dyson's own agent loop would use with the Anthropic
or OpenAI backends.

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

The `dangerous_no_sandbox` flag flows from CLI → Settings → ClaudeCodeClient → McpHttpServer. Currently a no-op for MCP tool calls (workspace tools are in-memory), but plumbed for future tools that need sandboxing.

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

### Dyson as MCP Server (serve.rs)

Dyson **serves** the unified workspace tool to Claude Code via an HTTP MCP server.

```
Claude Code agent loop → HTTP → McpHttpServer → WorkspaceTool.run() → workspace
```

Automatic when `provider: "claude_code"` + workspace is configured.  Only used
with the Claude Code backend (Anthropic/OpenAI backends use Dyson's own tool
execution).

### Both at once

Both directions can be active simultaneously.  For example, with a Claude Code
backend and MCP servers configured:

- Dyson connects to the GitHub MCP server as a client (McpSkill)
- Dyson serves workspace tools to Claude Code as a server (McpHttpServer)
- Claude Code has Bash, Read, Write (built-in) + GitHub tools (MCP client-side,
  forwarded via McpSkill's system prompt) + workspace tools (MCP server-side)

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
