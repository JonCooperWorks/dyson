# MCP HTTP Server (`skill/mcp/serve`)

## Overview
Server side of the Model Context Protocol. When Dyson uses Claude Code as its LLM backend, Claude Code runs its own agent loop and cannot receive Dyson's tools directly, so Dyson starts a loopback JSON-RPC 2.0 MCP server and passes it to Claude Code via `--mcp-config`. The server exposes the unified `workspace` tool (plus forwarded non-agent-only tools), workspace files as resources, and workspace skills as prompts.

## Interface
- `McpHttpServer` [mod.rs:McpHttpServer]
  - `new(workspace, extra_tools)` - workspace tool plus extras [mod.rs:McpHttpServer::new]
  - `new_tools_only(extra_tools)` [mod.rs:McpHttpServer::new_tools_only]
  - `bearer_token()` - token to hand to Claude Code [mod.rs:McpHttpServer::bearer_token]
  - `start(self: Arc<Self>) -> Result<(u16, JoinHandle<()>, String)>` - binds and serves, returning port, task handle, token [mod.rs:McpHttpServer::start]
- JSON-RPC methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`, `resources/list`, `resources/read`, `prompts/list`, `prompts/get`, `completion/complete` [mod.rs:dispatch]
- Resource URI scheme `workspace://` [mod.rs:WORKSPACE_URI_SCHEME]

## Invariants
- Binds `127.0.0.1:0` only (loopback, OS-assigned port) [mod.rs:McpHttpServer::start]
- Every request must present `Authorization: Bearer <token>`; the token comes from `BearerTokenAuth::generate()` (32 CSPRNG bytes, 64 hex chars) [mod.rs:McpHttpServer::new]
- At most `MAX_CONCURRENT_CONNECTIONS = 64` connections, enforced by a semaphore [mod.rs:MAX_CONCURRENT_CONNECTIONS]
- `REQUEST_TIMEOUT = 30 s` [mod.rs:REQUEST_TIMEOUT]; request bodies capped at 10 MB (`MAX_REQUEST_BODY`) by `content-length` and by actual bytes [mod.rs:handle_request]
- Tool map is populated once at construction and never modified [mod.rs:McpHttpServer]
- Completion results are truncated to `MAX_COMPLETION_VALUES = 100` [mod.rs:MAX_COMPLETION_VALUES]

## Side Effects
- Opens a TCP listener and spawns a Tokio accept task plus one task per connection [mod.rs:McpHttpServer::start]
- `tools/call` runs Dyson tools against the shared workspace (may write workspace files) [mod.rs:handle_tools_call]
- Reads `skills/<name>/SKILL.md` from disk for `prompts/list` / `prompts/get` [mod.rs:handle_prompts_get, mod.rs:skill_description]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
