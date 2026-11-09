# Web SPA API Layer (`web/src/api`)

## Overview
Browser-side transport for the Dyson web UI. `auth.js` bootstraps authentication (OIDC Authorization Code + PKCE when the backend advertises it), `client.js` wraps every HttpController endpoint used by the SPA plus the per-conversation SSE stream, `stream.js` parses and dispatches SSE events as pure functions, and `boot.js` performs the cold load and installs the conversation polling loop.

## Interface
- `loadAuthConfig()` - fetches `GET /api/auth/config` to learn the active auth mode [auth.js:loadAuthConfig]
- `bootstrapAuth()` - runs the auth flow (redirect / callback / silent refresh) and resolves a session exposing `getToken` [auth.js:bootstrapAuth]
- `DysonClient` - HTTP + SSE client; constructor takes injectable `fetch`, `EventSource`, `getToken` [client.js:DysonClient]
  - Conversations: `listConversations`, `createChat`, `load`, `deleteChat`, `clearConversation` (POSTs `/clear` to `/api/conversations/:id/turn`), `exportConversation` [client.js:DysonClient]
  - Turns/runs: `send`, `attach`, `cancel`, `getRun`, `pauseRun`, `resumeRun` (`/api/conversations/:id/{turn,events,cancel,run,pause,resume}`) [client.js:DysonClient]
  - Providers/models: `listProviders`, `listModels`, `postModel` (`/api/model`), provider OAuth helpers on `/api/provider-auth/:provider[/complete]` [client.js:DysonClient]
  - Other endpoints: `/api/commands`, `/api/mcp/servers`, `/api/mcp/elicitations[/:id]`, `/api/mind`, `/api/mind/file`, `/api/agent`, `/api/activity`, `/api/audit`, `/api/conversations/:id/{feedback,artefacts,artefacts/:id}` [client.js:DysonClient]
- `parseStreamEvent(raw)` / `dispatchStreamEvent(msg, callbacks)` - SSE wire parsing for event types `text`, `thinking`, `tool_start`, `tool_result`, `checkpoint`, `file`, `user_message`, `artefact`, `title`, `llm_error`, `done` [stream.js:parseStreamEvent]
- `boot(client, { pollMs = 10_000 })` - cold load + conversation poll; returns a disposer [boot.js:boot]

## Invariants
- Every authenticated request carries `x-dyson-csrf: 1` and, when a token exists, `authorization: Bearer <token>`; the controller's CSRF gate rejects requests without it [client.js:_authedFetch]
- Authenticated fetches refuse protocol-relative (`//`) and cross-origin absolute URLs [client.js:assertSameOriginAuthenticatedUrl]
- When a token is present, `POST /api/auth/sse-ticket` must succeed before the `/events` EventSource is opened [client.js:_openEvents]
- The EventSource is closed when a `done` event arrives [client.js:_openEvents]
- Tokens are refreshed `REFRESH_LEEWAY_S = 60` seconds before expiry [auth.js:REFRESH_LEEWAY_S]
- `stream.js` is pure: no network or EventSource access [stream.js:parseStreamEvent]

## Side Effects
- Stores OIDC tokens and pending PKCE state in `sessionStorage` under `dyson:auth` and `dyson:auth:pending` [auth.js:writeTokens, auth.js:writePending]
- Network calls to `/api/auth/config` and to the IdP `token_endpoint` [auth.js:loadAuthConfig, auth.js:handleCallback]
- Opens an `EventSource` with `withCredentials: true` per conversation stream [client.js:_openEvents]
- `boot` mutates the app store (`setLive`, `setConversations`, `setProviders`, ...), auto-creates a "New conversation" when the list is empty, and installs a `setInterval` poll [boot.js:boot]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
