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

Dyson has workspace tools that give the agent identity and memory:

| Tool | Purpose |
|------|---------|
| `workspace_view` | Read workspace files (SOUL.md, MEMORY.md, journals, etc.) |
| `workspace_search` | Search across workspace files by pattern |
| `workspace_update` | Write/append to workspace files |

With the Anthropic or OpenAI backends, these tools go through Dyson's own agent
loop — the LLM emits `tool_use` blocks, Dyson executes them, and sends back
`tool_result`.  Simple.

But with the Claude Code backend, Claude Code **is** the agent.  Dyson just
streams its output.  Claude Code has no concept of Dyson's workspace tools.
We can't shove them into Claude Code's tool list — it manages its own tools
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
│  │       "url":        │     │   │  → workspace_view    │   │
│  │       "http://...   │     │   │  → workspace_search  │   │
│  │       /mcp"}}}'     │     │   │  → workspace_update  │   │
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
   (loopback-only, OS-assigned port) and spawns a tokio task for the accept loop.

2. **Build config JSON**: Construct the MCP server config that tells Claude Code
   where to connect:
   ```json
   {
     "mcpServers": {
       "dyson-workspace": {
         "type": "url",
         "url": "http://127.0.0.1:54321/mcp"
       }
     }
   }
   ```

3. **Pass to Claude Code**: The config is passed as a CLI argument:
   ```
   claude -p --mcp-config '{"mcpServers":{"dyson-workspace":{"type":"url","url":"http://127.0.0.1:54321/mcp"}}}'
   ```

4. **Claude Code connects**: During startup, Claude Code reads the MCP config,
   connects to our HTTP server, and runs the MCP handshake.

5. **Tools available**: Claude Code now has `workspace_view`, `workspace_search`,
   and `workspace_update` as first-class structured tools with proper JSON
   schemas.  The LLM can call them just like Bash, Read, or Write.

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
    │◀─ 200 OK ───────────────────│  {"result": {"tools": [workspace_view, ...]}}
    │                              │
    │   ... during agent loop ...  │
    │                              │
    │── POST /mcp ────────────────▶│  {"method": "tools/call", "params": {"name": "workspace_view", ...}}
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

The tools are not duplicated.  `McpHttpServer` uses the exact same
`WorkspaceViewTool`, `WorkspaceSearchTool`, and `WorkspaceUpdateTool`
that Dyson's own agent loop would use with the Anthropic or OpenAI backends.

### Lifecycle

The MCP server's lifetime is tied to the LLM stream:

```
stream() called
  └─ McpHttpServer starts (tokio task)
  └─ claude -p spawned
  └─ async_stream closure holds:
       - child process (stdin/stdout)
       - JoinHandle for MCP server task
  └─ Stream consumed by agent loop...
  └─ Turn complete (or cancelled)
  └─ Stream dropped
       └─ JoinHandle dropped → task aborted → server stops
       └─ Child process stdin closed → process exits
```

A new server is created per LLM turn.  This is fine: binding a TCP socket
is ~0.1ms, and each turn takes seconds.  Per-turn servers simplify lifecycle:
no shutdown coordination, no stale connections, no port leaks.

---

## Security

- **Loopback-only**: The server binds to `127.0.0.1`, not `0.0.0.0`.  Only
  local processes can reach it.  No network exposure.

- **No authentication**: The server has no auth.  This is acceptable because:
  - Only the co-located `claude -p` subprocess connects
  - Binding is loopback-only (no remote access)
  - The port is ephemeral and short-lived (one LLM turn)

- **No secrets in URL**: The MCP config passed via CLI args contains only
  a loopback URL.  No API keys, tokens, or credentials.

---

## Sandbox Plumbing

The `dangerous_no_sandbox` flag flows through the entire chain:

```
CLI (--dangerous-no-sandbox)
  → Settings.dangerous_no_sandbox
    → create_client(settings, workspace, dangerous_no_sandbox)
      → ClaudeCodeClient.dangerous_no_sandbox
        → McpHttpServer.dangerous_no_sandbox
          → (future) sandbox.check() before tool.run()
```

Today this flag has **no effect** on MCP tool calls.  Workspace tools are
pure in-memory operations (reading/writing a HashMap behind an RwLock) that
don't need sandboxing.

The hook is here so that when we add tools that touch the filesystem or
execute commands via MCP, we can gate them through the sandbox system without
changing any APIs, types, or call sites.  The plumbing is done; only the
enforcement logic needs to be added.

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

Dyson **serves** workspace tools to Claude Code via an HTTP MCP server.

```
Claude Code agent loop → HTTP → McpHttpServer → WorkspaceViewTool.run() → workspace
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

- **MockWorkspace** provides a minimal in-memory workspace implementation
  with one file for verifying tool execution end-to-end.

Run with:
```bash
cargo test skill::mcp::serve
```
