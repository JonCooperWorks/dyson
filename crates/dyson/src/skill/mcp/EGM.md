# MCP Client Skill (`skill/mcp`)

## Overview
Model Context Protocol client support. `McpSkill` connects to a configured MCP server over a stdio subprocess or HTTP, performs the `initialize` handshake, and exposes the server's tools (plus resource, prompt, and OAuth-submit helper tools) to the agent. Inbound server-originated messages are handled by `NotificationRouter`; `elicitation/create` requests are parked in a process-global broker answered from the web UI. The server side (Dyson as an MCP server for Claude Code) is in `serve/`.

## Interface
- `McpSkill` - implements `Skill`; `new(config)`, `config_name`, `server_display_name`, `server_version`, `server_instructions`, `with_sampling_context` [mod.rs:McpSkill]
- `transport` (pub): `McpTransport` trait, `InboundHandler` trait, `UnsupportedInboundHandler`, `StdioTransport::spawn(command, args, env, sandbox, deny_network)`, `HttpTransport` [transport.rs:McpTransport, transport.rs:StdioTransport, transport.rs:HttpTransport]
- `protocol` (pub): re-exports `dyson_common::jsonrpc` types; `JsonRpcResponseExt`, `McpToolDef`, `McpToolResult`, `McpContent`, `McpResourceContents`, `McpResourceInfo`, `McpPromptInfo`, `ServerCapabilities`, `InitializeResult`, `ServerInfo` [protocol.rs:McpToolDef, protocol.rs:InitializeResult]
- `router` (pub): `NotificationRouter` (`InboundHandler`), `SamplingBackend` [router.rs:NotificationRouter]
- `elicitation` (pub): `ElicitationBroker`, `broker()`, `enable_ui()`, `ui_enabled()` [elicitation.rs:broker]
- `serve` (pub): [serve](./serve/EGM.md) - loopback MCP server exposing workspace tools
- Internal tools: `McpRemoteTool` [remote_tool.rs:McpRemoteTool], `McpResourcesTool` [resources.rs:McpResourcesTool], `McpPromptsTool` [prompts.rs:McpPromptsTool], `OAuthSubmitTool` [oauth_flow.rs:OAuthSubmitTool]
- OAuth: `start_oauth_flow`, `load_oauth_credential`, `OAuthPending`, `extract_code` [oauth_flow.rs:start_oauth_flow]
- Artefacts: `save_mcp_image`, `save_mcp_resource`, `MAX_MCP_RESOURCE_BYTES`, `INLINE_TEXT_CAP` [artifacts.rs:save_mcp_resource]

## Invariants
- Request frames are capped at `MAX_MCP_REQUEST_BYTES = 16 MiB` and results at `MAX_MCP_RESULT_BYTES = 64 MiB` (by `content-length`, streamed chunks, and stdio line reads) [transport.rs:MAX_MCP_REQUEST_BYTES, transport.rs:MAX_MCP_RESULT_BYTES]
- With `sandbox = true` on Linux with `bwrap` on PATH, stdio servers run under bwrap with a read-only root, and `deny_network` unshares the network namespace; otherwise it falls back unsandboxed with a warning [transport.rs:StdioTransport::spawn, transport.rs:resolve_exec_with]
- HTTP transport echoes a server-returned `Mcp-Session-Id` on later requests [transport.rs:HttpTransport]
- Server-originated sampling is not advertised or serviced by default: MCP servers are untrusted [mod.rs:McpSkill]
- Server descriptions are stripped of control characters and capped at 500 chars; instructions at 2000 chars [text.rs:sanitize_mcp_description, text.rs:sanitize_mcp_instructions]
- The `elicitation` capability is advertised only after `enable_ui()`; otherwise `elicitation/create` gets `-32601`. Parked elicitations time out after 300 s [elicitation.rs:enable_ui, elicitation.rs:ELICITATION_TIMEOUT]
- Async MCP tasks: TTL 300,000 ms and max wait 300 s [remote_tool.rs:MCP_TASK_TTL_MS, remote_tool.rs:MCP_TASK_MAX_WAIT]
- Resource blobs/text over 64 MiB are rejected; inline text capped at 16 KiB [artifacts.rs:MAX_MCP_RESOURCE_BYTES, artifacts.rs:INLINE_TEXT_CAP]

## Side Effects
- Spawns MCP server subprocesses and background reader tasks [transport.rs:StdioTransport::spawn]
- Outbound HTTP to MCP servers with configured `Auth` headers [transport.rs:HttpTransport]
- OAuth: starts a loopback callback server (`http://127.0.0.1:{port}/callback`, 300 s), persists/loads tokens via `crate::auth::oauth`, and touches the config file mtime to trigger hot reload [oauth_flow.rs:start_oauth_flow, oauth_flow.rs:touch_config]
- Writes MCP images/resources to `std::env::temp_dir()` [artifacts.rs:save_mcp_image, artifacts.rs:save_mcp_resource]
- Process-global elicitation broker and `UI_ENABLED` flag [elicitation.rs:broker]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
