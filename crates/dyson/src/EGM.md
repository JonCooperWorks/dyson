# Dyson Crate Source Blueprint (`dyson/crates/dyson/src`)

## System Overview
`dyson` is a streaming, composable AI agent: an LLM streams tool calls in a loop until it has an answer, and everything else (built-in tools, MCP servers, workspace skills, subagents) plugs into that loop through traits. The library (`lib.rs`) exposes the agent loop, LLM clients, sandbox, skills/tools, workspace, configuration, controllers (HTTP web UI, Telegram, terminal), and Swarm integration. The `dyson` binary (`main.rs`) is a clap CLI dispatching to `command/`. Several modules are facades over sibling workspace crates (`dyson-core`, `dyson-harness`, `dyson-ast`, `dyson-dependency-analysis`, `dyson-persistence`, `dyson-common`).

## Local Interface & Invariants
- `lib.rs` declares `pub mod` `advisor`, `agent`, `ast`, `auth`, `chat_history`, `config`, `controller`, `dependency_analysis`, `error`, `export`, `feedback`, `http`, `llm`, `media`, `message`, `message_cost_backfill`, `sandbox`, `secret`, `security_spawns`, `skill`, `subscription_models`, `swarm_cost`, `swarm_state_sync`, `telemetry`, `tokens`, `tool`, `workspace`; `pub(crate)` `tool_hooks`, `util`; re-exports `DysonError`, `Result` [lib.rs]
- `main.rs` - CLI subcommands `listen`, `init`, `hash-bearer`, `cost-backfill`, `swarm`, `run` [main.rs:Commands]; installs the rustls ring provider, logs to `~/.dyson`, initialises telemetry [main.rs:main]
- `error`, `message` - facades `pub use dyson_core::{error,message}::*` [error.rs, message.rs]
- `http` - shared `client()` (connect 10 s, read 120 s, request 300 s, at most 4 redirects), `safe_redirect_policy`, `strict_untrusted_redirect_policy`, `validate_url_safe` / `verify_url_safe` (reject private IPv4/IPv6 literals and cloud metadata hosts), plus redirect policies with the same checks, `pinned_client_for_validated_url`, `ensure_crypto_provider`, re-exported `is_private_v4/v6`, `host_from_url` [http.rs:client, http.rs:validate_url_safe, http.rs:MAX_REDIRECTS]
- `feedback` - `FeedbackStore` writing `{dir}/{chat_id}/feedback.json`; re-exports `FeedbackEntry`, `FeedbackRating` [feedback.rs:FeedbackStore]
- `swarm_state_sync` - background worker mirroring durable state files to Swarm every 5 s (files <= 5 MiB, backoff up to 10 min) via `/v1/internal/state/file`; `spawn_worker`, `config_from_env`, `persist_model_selection` (`/v1/internal/model-selection`) [swarm_state_sync.rs:spawn_worker, swarm_state_sync.rs:SYNC_INTERVAL, swarm_state_sync.rs:MAX_SYNC_FILE_BYTES]
- `swarm_cost` - display-only lookup of Swarm cost rows (2 s timeout); Swarm remains the billing source of truth [swarm_cost.rs:lookup_runtime_display_metadata, swarm_cost.rs:COST_LOOKUP_TIMEOUT]
- `message_cost_backfill` - `backfill_history`, `CostBackfillOptions`, `CostBackfillReport`, `merge_cost` [message_cost_backfill.rs:backfill_history]
- `security_spawns` - reports tool-shell spawns (ids and names only, never commands) to Swarm `/v1/internal/security/tool-spawns`; queue max 2000, batch 200, flush every 2 s, best effort; `correlate`, `record` [security_spawns.rs:record, security_spawns.rs:MAX_QUEUED]
- `telemetry` - OpenTelemetry tracing (`init`, `layer`, `inject_proxy_context`); disabled by `OTEL_SDK_DISABLED=true`; never exports application log events [telemetry.rs:init]
- `tokens` - typed Swarm bearers `pt_` (LLM proxy), `it_` (artefact ingest), `st_` (state sync) re-exported from `dyson_common::tokens` [tokens.rs]
- `subscription_models` - `CHATGPT`, `CLAUDE` model catalogues [subscription_models.rs:CHATGPT]
- `tool_hooks` (crate) - `ToolHook`, `ToolHookEvent`, `HookDecision`, `dispatch_hooks` [tool_hooks.rs:dispatch_hooks]
- `util` (crate) - `MAX_OUTPUT_BYTES = 100 KiB`, `truncate_output`, `resolve_tilde`, `backoff_with_jitter` [util.rs:MAX_OUTPUT_BYTES]

## Subdirectories & System Boundaries
- advisor: [./advisor/EGM.md](./advisor/EGM.md) - native Anthropic or generic child-agent advisor
- agent: [./agent/EGM.md](./agent/EGM.md) - the streaming tool-use loop, dreams, continuations, task budgets/leases
- ast: [./ast/EGM.md](./ast/EGM.md) - facade over `dyson-ast`
- auth: [./auth/EGM.md](./auth/EGM.md) - `Auth` trait: API key, bearer, Argon2 bearer, OAuth, OIDC
- chat_history: [./chat_history/EGM.md](./chat_history/EGM.md) - persistence facade and factory (`disk`)
- command: [./command/EGM.md](./command/EGM.md) - CLI subcommands (binary only)
- config: [./config/EGM.md](./config/EGM.md) - `Settings`, loader, migration, hot reload
- controller: [./controller/EGM.md](./controller/EGM.md) - controllers, outputs, HTTP and Telegram transports
- dependency_analysis: [./dependency_analysis/EGM.md](./dependency_analysis/EGM.md) - facade over `dyson-dependency-analysis`
- export: [./export/EGM.md](./export/EGM.md) - ShareGPT conversation export
- llm: [./llm/EGM.md](./llm/EGM.md) - provider clients, SSE parsing, retries, registry, pricing
- media: [./media/EGM.md](./media/EGM.md) - image/audio/PDF/Office attachment ingestion
- sandbox: [./sandbox/EGM.md](./sandbox/EGM.md) - policy, OS (bwrap/container), and Cedar sandboxes
- secret: [./secret/EGM.md](./secret/EGM.md) - secret references and resolvers
- skill: [./skill/EGM.md](./skill/EGM.md) - `Skill` trait, builtin, local, MCP, subagent skills
- tool: [./tool/EGM.md](./tool/EGM.md) - `Tool` trait and built-in tools
- workspace: [./workspace/EGM.md](./workspace/EGM.md) - identity, memory, journals, FTS5 memory store

## Global Invariants
- Every tool call is gated by the sandbox; the sandbox can be disabled only with `--dangerous-no-sandbox`, and startup aborts when the OS backend is missing [sandbox/mod.rs:create_sandbox]
- Subagents inherit the parent sandbox and are capped at `MAX_SUBAGENT_DEPTH = 3` [tool/mod.rs:MAX_SUBAGENT_DEPTH]
- Outbound fetches of untrusted URLs go through `http::validate_url_safe` (SSRF guard) [http.rs:validate_url_safe, tool/web_fetch.rs]
- `/api/*` requests require auth (bearer, hashed bearer, Swarm, or OIDC) except on loopback `DangerousNoAuth`; state-changing requests require `X-Dyson-CSRF` [controller/http/routes/mod.rs:authorize_route]
- Secrets are held in zeroizing `Credential`s and never sent over plain HTTP to non-local hosts [auth/credential.rs:Credential, config/loader/secrets.rs:reject_http_with_api_key]

## Global Side Effects
- LLM provider HTTPS APIs and `claude`/`codex` subprocesses [llm/EGM.md]
- Local filesystem: `dyson.json`, workspace, chat history, `~/.dyson` logs and OAuth tokens [config/EGM.md, workspace/EGM.md, auth/EGM.md]
- Swarm internal APIs: state sync, cost lookup, security spawn reports, artefact push, provider-auth relay [swarm_state_sync.rs, swarm_cost.rs, security_spawns.rs]
- Network listeners: HTTP controller, loopback MCP server, OAuth callback server [controller/http/EGM.md, skill/mcp/serve/EGM.md, auth/EGM.md]
- Telegram Bot API [controller/telegram/EGM.md]

## System Verification
- Run: `cargo test -p dyson` (from `dyson/`)
- Web UI: `npm test` (in `controller/http/web`)
