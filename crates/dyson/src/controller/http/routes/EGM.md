# HTTP Controller Routes (`controller/http/routes`)

## Overview
Request dispatch for the Dyson HTTP controller. `mod.rs` normalises the path into segments, applies Host, auth, SSE-ticket, configure-secret, and CSRF gates for `/api/*`, then dispatches one match arm per (method, segments) tuple to per-feature handler modules. Handlers are `pub(super)` and return the boxed `Resp` alias from `../responses.rs`.

## Interface
- `dispatch(req, state)` - router entry point [mod.rs:dispatch]
- Unauthenticated: `GET /healthz`, `GET /readyz`, `GET /api/auth/config`; `POST /api/auth/sse-ticket` (needs bearer); `POST /webhook/telegram` (validated by auth snapshot) [mod.rs:dispatch_inner]
- Conversations (`/api/conversations[/:id]`): list/create/get/delete [conversations.rs:list, conversations.rs:create, conversations.rs:get, conversations.rs:delete]; `POST :id/cancel` [conversations.rs:cancel]
- Turns: `POST /api/conversations/:id/turn` [turns.rs:post]; `POST :id/resume` and `:id/signal` [runs.rs:resume]; `GET :id/run`, `POST :id/pause` [runs.rs:get, runs.rs:pause]; `coalesce_queued` [turns.rs:coalesce_queued]
- SSE: `GET /api/conversations/:id/events` [sse.rs:events]
- Recovery: `GET|POST /api/conversations/:id/recovery` [recovery.rs:get, recovery.rs:post]
- Feedback: `GET|POST /api/conversations/:id/feedback` [feedback.rs:get, feedback.rs:post]
- Artefacts/files: `GET :id/artefacts`, `:id/artefacts/:aid`, `:id/export`, `:id/files/:fid`, `/api/artefacts/:id`, `/api/files/:id`, `/artefacts/:id` [artefacts.rs:list, artefacts.rs:get_for_chat, artefacts.rs:export, files.rs:get_for_chat]
- Providers/models: `GET /api/providers` [providers.rs:list], `GET /api/models` [models.rs:list], `POST /api/model` [model.rs:post], `/api/provider-auth/:provider[/complete]` [provider_auth.rs:status, provider_auth.rs:start, provider_auth.rs:forget, provider_auth.rs:complete]
- Agent/workspace: `GET /api/agent` [agent.rs:get], `GET /api/commands` [commands.rs:get], `GET /api/mind`, `GET|POST /api/mind/file` [mind.rs:get, mind.rs:get_file, mind.rs:post_file]
- Observability: `GET /api/activity` [activity.rs:get], `GET /api/audit` [audit.rs:get]
- `debug_log.rs` (`tail`, `TAIL_BYTES`) is present on disk but is not declared as a module in `mod.rs`, so it is not compiled or routed [mod.rs:dispatch]
- MCP: `GET /api/mcp/servers` [mcp.rs:list_servers], `GET /api/mcp/elicitations`, `POST /api/mcp/elicitations/:id` [elicitation.rs:list, elicitation.rs:respond]
- Admin (`/api/admin/{configure,state/file,skills/install,skills/:skill,skills,idle,quiesce,unquiesce,cost-backfill}`) [admin.rs:post] - handlers in [./admin/EGM.md](./admin/EGM.md)
- Static SPA shell [static_assets.rs:serve]
- Helpers: `source_for_chat_id`, `bump_to_front`, `first_user_text`, `message_to_dto` [conversations.rs:source_for_chat_id, conversations.rs:message_to_dto]

## Invariants
- Gates and dispatch share one normalised segment view so variants like `//api/...` cannot bypass auth [mod.rs:dispatch_inner]
- When `loopback_only_host_check` is set, `/api/*` requests require Host `127.0.0.1`, `::1`, or `localhost` [mod.rs:loopback_host_allowed]
- Only the listed configure-admin routes in `SwarmBearer` mode skip bearer auth and are instead validated by `authorize_configure`; `cost-backfill` is never exempt [mod.rs:authorize_route]
- State-changing methods (POST/DELETE/PUT/PATCH) on `/api/*` except `sse-ticket` require the `X-Dyson-CSRF` header [mod.rs:authorize_route]
- SSE tickets are delivered as the `dyson_sse` cookie and consumed once [mod.rs:SSE_TICKET_COOKIE, mod.rs:consume_sse_ticket]
- `/readyz` returns 503 while a readiness error (e.g. warmup placeholder config) is active [mod.rs:dispatch_inner]
- Body caps: `MAX_TURN_BODY` checked against `content-length` [turns.rs:post], `MAX_CONFIGURE_BODY` 64 KiB [admin.rs:MAX_CONFIGURE_BODY], `MAX_ELICITATION_BODY` 64 KiB [elicitation.rs:MAX_ELICITATION_BODY], `MAX_COMPLETE_BODY` 16 KiB [provider_auth.rs:MAX_COMPLETE_BODY]
- SSE idle timeout 300 s [sse.rs:IDLE_TIMEOUT]; MCP probe timeout 5 s [mcp.rs:PROBE_TIMEOUT]; model list cache TTL 30 min [models.rs:CACHE_TTL]
- `audit` forwards only `range`, `since`, `until`, `limit` query params [audit.rs:FORWARDED_PARAMS]
- Provider OAuth credentials never cross the relay; only coarse state and one-time URLs/codes [provider_auth.rs:relay]

## Side Effects
- Spawns background agent turn tasks (`tokio::spawn`) [turns.rs:post]
- Turn and resume admission return 503 `instance is quiesced for maintenance` while quiesced [turns.rs:post, turns.rs:resume]
- `admin::post` updates `IDENTITY.md`, commits one config patch, and reloads settings [admin.rs:post]
- Reads artefact metadata/body files from disk [artefacts.rs:list_for_chat]
- Writes workspace files via `POST /api/mind/file` [mind.rs:post_file]
- Outbound HTTP to Swarm for provider auth relay [provider_auth.rs:relay] and to model-list endpoints [models.rs:list]
- Connects to MCP servers to probe them [mcp.rs:list_servers]; responds to the process-global elicitation broker [elicitation.rs:respond]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
