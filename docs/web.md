# Web UI / HTTP Controller

> ## ⚠️ Loopback only.  Do not expose this to the public internet.
>
> The HTTP controller has two inbound-auth modes: `bearer` (shared
> token on every `/api/*` request) and `dangerous_no_auth` (anonymous,
> every request accepted).  Either way, the controller is designed for
> a single trusted operator behind loopback or a VPN mesh.  Bind to
> `127.0.0.1` (the default) and reach it remotely via SSH tunnel or
> Tailscale — never via `0.0.0.0` on a host with public network
> exposure.  See [README — Web UI](../README.md#web-ui).

---

The HTTP controller serves a small web UI plus a JSON API + Server-Sent
Events stream.  It implements the same `Controller` trait as the
terminal and Telegram controllers and shares ChatHistory + FeedbackStore
with them, so a chat you started on Telegram shows up in the web UI and
vice versa.

```
┌──── browser ─────────┐               ┌──── dyson process ────────┐
│                      │               │                           │
│  React (Vite-built)  │ ◀──── SSE ───┤  HttpController           │
│  index.html          │ ──── HTTP ──▶ │  (hyper http1)            │
│  /assets/*.{js,css}  │               │                           │
│                      │               │   ┌────────────────────┐ │
└──────────────────────┘               │   │ shared with        │ │
                                       │   │ terminal/telegram: │ │
                                       │   │  Agent loop        │ │
                                       │   │  ChatHistory       │ │
                                       │   │  FeedbackStore     │ │
                                       │   │  Workspace         │ │
                                       │   │  ClientRegistry    │ │
                                       │   └────────────────────┘ │
                                       └───────────────────────────┘
```

## Configuration

Add to your `dyson.json` `controllers` array:

```json
{ "type": "http", "bind": "127.0.0.1:7878" }
```

| field | type | default | description |
|---|---|---|---|
| `bind` | `string` | `"127.0.0.1:7878"` | Address to bind.  Loopback-only is the only supported deployment.  Listening on `0.0.0.0` exposes the agent. |
| `webroot` | `string?` | `null` | Optional path to a Vite build output (`crates/dyson/src/controller/http/web/dist/`) to serve from disk instead of the embedded bundle.  Re-running `npm run build` in `web/` refreshes the files without recompiling dyson.  Most UI development should use `npm run dev` against a running binary instead — see [Developing the frontend](#developing-the-frontend) below. |
| `auth` | `object` | `dangerous_no_auth` on loopback, **required** otherwise | Inbound authentication.  See below. |

### Authentication

On a loopback bind (`127.0.0.1` or `::1`) the `auth` field is optional:
the loopback threat model is a single trusted operator, so an unset
field defaults to `dangerous_no_auth`.  On any other bind the field is
**required** — omitting it makes the controller refuse to start rather
than silently expose an unauthenticated endpoint.  This mirrors the
posture `--dangerous-no-sandbox` takes for the sandbox boundary.

Two variants today:

```json
{ "auth": { "type": "dangerous_no_auth" } }
```

Accepts every request as anonymous.  Opt-in escape hatch for local
development behind loopback — the controller logs a loud warning at
startup.

```json
{
  "auth": {
    "type": "bearer",
    "token": { "resolver": "insecure_env", "name": "DYSON_WEB_TOKEN" }
  }
}
```

Requires `Authorization: Bearer <token>` on every `/api/*` request;
mismatches return `401 {"error":"unauthorized"}`.  Static-shell paths
(`/`, `/assets/*`) are exempt so the UI can load before the browser
has presented the credential.  The `token` field flows through the
same secret-resolver pipeline that Telegram's `bot_token` uses, so it
can be a literal string or a `{ "resolver": …, "name": … }` reference.

Both variants are implementations of the shared `Auth` trait at
`crates/dyson/src/auth/mod.rs`, which is also used by the MCP server.
A future config variant can plug in any other `Auth` implementation
without touching `dispatch()`.

The web assets live in
[crates/dyson/src/controller/http/web/](../crates/dyson/src/controller/http/web/)
as a Vite + React project.  `crates/dyson/build.rs` runs `npm run
build` on every cargo build (gated by source-file mtimes, so nothing
fires when the frontend is untouched), walks the resulting `dist/`,
and generates an asset table that `include_bytes!`s every file into
the binary — so a deployed binary still needs nothing but itself.

## Surfaces

Three top-level views, switched via top-bar nav or `⌘1`/`⌘2`/`⌘3`:

- **Conversations** — left rail lists chats (newest-first), centre is
  the active transcript with markdown rendering, right rail is the tool
  stack (collapsible — click the plug icon).
- **Mind** — workspace browser; click any file to load it in the
  editor; `⌘S` saves back to the workspace via the same path the agent
  uses for its own writes.
- **Activity** — background loops, dreams, swarm tasks.  Currently
  empty pending cross-controller `BackgroundAgentRegistry` aggregation.

The model picker in the top-bar lists every provider from `dyson.json`
as a collapsible group (active provider open by default) with each
configured model under it.  Selecting a model calls `Agent::swap_client`
on the active chat — same path the Telegram `/model` command takes.

## API

All endpoints return JSON unless noted.  Errors are
`{ "error": "<message>" }` with a non-2xx status.

### Conversations

| Method | Path | Body | Returns |
|---|---|---|---|
| `GET` | `/api/conversations` | — | `[ConversationDto]` newest-first |
| `POST` | `/api/conversations` | `{ title? }` | `{ id, title }` |
| `GET` | `/api/conversations/:id` | — | `{ id, title, messages: [MessageDto] }` |
| `POST` | `/api/conversations/:id/turn` | `{ prompt }` | `202 { ok: true }` — events stream via SSE |
| `POST` | `/api/conversations/:id/cancel` | — | `{ ok: true }` |
| `GET` | `/api/conversations/:id/events` | — | `text/event-stream` of `SseEvent` |
| `GET` | `/api/conversations/:id/feedback` | — | `[FeedbackEntry]` |
| `POST` | `/api/conversations/:id/feedback` | `{ turn_index, emoji }` | `{ ok, rating? }` (empty `emoji` removes) |

`ConversationDto`:
```json
{ "id": "c-0001", "title": "audit auth.rs", "live": false }
```

`MessageDto.blocks[*]` discriminator is `type`:
- `text` — `{ type: "text", text }`
- `thinking` — `{ type: "thinking", thinking }`
- `tool_use` — `{ type: "tool_use", id, name, input }`
- `tool_result` — `{ type: "tool_result", tool_use_id, content, is_error }`

### Server-Sent Events

`GET /api/conversations/:id/events` is a long-lived SSE stream.  Frames
are `data: <json>\n\n`.  Discriminator is `type`:

| `type` | Payload |
|---|---|
| `text` | `{ delta: string }` |
| `tool_start` | `{ id, name }` |
| `tool_result` | `{ content, is_error, view? }` — see "Tool views" below |
| `checkpoint` | `{ text }` |
| `llm_error` | `{ message }` |
| `done` | `{}` (always last; client should `close()` after this) |

Send a turn after subscribing, not before — opening the SSE first is
the only way to avoid missing the first deltas.

### Tool views

`tool_result.view` is an optional typed payload that the right-rail
panel renders natively.  Discriminator is `kind`:

| `kind` | Producer | Payload (additional fields) |
|---|---|---|
| `bash` | `bash` | `{ lines: [{c,t}], exit_code, duration_ms }` |
| `diff` | `edit_file` / `write_file` / `bulk_edit` | `{ files: [{path,add,rem,hunk,rows: [{t,ln,sn,l}]}] }` |
| `sbom` | `dependency_scan` | `{ rows: [{pkg,ver,sev,id,reach,note}], counts }` |
| `taint` | `taint_trace` | `{ flow: [{kind,loc,sym,note}] }` |
| `read` | `read_file` | `{ path, lines: [string], highlight? }` |

Tools that don't attach a view are rendered with the plain-text
fallback panel.  See
[crates/dyson/src/tool/view.rs](../crates/dyson/src/tool/view.rs).

### Providers

```
GET /api/providers
[
  {
    "id": "default",
    "name": "default",
    "models": ["qwen/qwen3.6-plus", "minimax/minimax-m2.5", ...],
    "active_model": "qwen/qwen3.6-plus",
    "active": true
  }
]

POST /api/model
{ "provider": "default", "model": "minimax/minimax-m2.5", "chat_id"?: "c-0001" }
→ { "ok": true, "provider": "...", "model": "...", "swapped": <count> }
```

`active` is sorted first.  Omit `chat_id` to swap on every loaded chat.

### Mind / workspace

```
GET /api/mind
→ { "backend": "filesystem", "files": [{ "path": "SOUL.md", "size": 2182 }, ...] }

GET /api/mind/file?path=SOUL.md
→ { "path": "SOUL.md", "content": "..." }

POST /api/mind/file
{ "path": "SOUL.md", "content": "..." }
→ { "ok": true, "path": "SOUL.md" }
```

The agent sees edits the next time it reads the file — same channel
the agent's own `workspace` tool writes through.

### Feedback (Telegram-equivalent)

```
POST /api/conversations/:id/feedback
{ "turn_index": 1, "emoji": "👍" }
→ { "ok": true, "rating": "good", "emoji": "👍" }
```

Emoji set is verbatim from
[crates/dyson/src/controller/telegram/feedback.rs](../crates/dyson/src/controller/telegram/feedback.rs):

| rating | emojis |
|---|---|
| Terrible (-3) | 💩 😡 🤮 |
| Bad (-2) | 👎 |
| NotGood (-1) | 😢 😐 |
| Good (+1) | 👍 👏 |
| VeryGood (+2) | 🔥 🎉 😂 |
| Excellent (+3) | ❤️ 🤯 💯 ⚡ |

Pass `"emoji": ""` to remove existing feedback for a turn.  Stored at
`{chat_history.connection_string}/{chat_id}_feedback.json` — the same
file Telegram writes to.

## Persistence

| Data | Where | When written |
|---|---|---|
| Chat transcript | `{chat_history.connection_string}/{chat_id}.json` | After every turn (success or error) |
| Feedback | `{chat_history.connection_string}/{chat_id}_feedback.json` | On `POST /feedback` |
| Workspace files | `{workspace.path}/{file}` | On `POST /api/mind/file` or whenever the agent calls `workspace` |
| Provider/model config | `dyson.json` | Manual — UI changes via `/api/model` are **session-local**; reload of the controller resets to dyson.json defaults |

The HTTP controller hydrates its in-memory chat list from the
ChatHistory directory on startup, sorted newest-first by file mtime.

## Static assets

Shipped as a bundled React app — sources under `web/src/`, built by
Vite into `web/dist/`, embedded in the binary by
[`crates/dyson/build.rs`](../crates/dyson/build.rs) which writes the
asset table to
[`assets.rs`](../crates/dyson/src/controller/http/assets.rs) via
`include!` at compile time.  Production bundle is one hashed JS chunk +
one hashed CSS chunk + `index.html`, served from `/` and `/assets/*`.

Override with `webroot:` pointing at `web/dist/` to load from disk
without recompiling dyson — handy after running `npm run build`.
Prefer `npm run dev` (below) for active UI work; HMR avoids both the
Rust rebuild and the Vite production build.

## Developing the frontend

The frontend has its own Node toolchain so it can be iterated on
independently.  From `crates/dyson/src/controller/http/web/`:

```bash
npm install           # once, or whenever package-lock.json changes
npm run dev           # Vite dev server on :5173, HMR, proxies /api to :7878
npm test              # vitest suite (regression checks for past bugs)
npm run build         # production bundle → dist/  (runs vitest first)
```

`npm run dev` assumes a dyson binary is running on `:7878` — the dev
server proxies `/api` and `/artefacts` to it, so the live API is a
TCP hop away while the UI reloads instantly on save.

`build.rs` invokes `npm run build` during `cargo build`, gated by
mtime on sources in `web/src/`, `index.html`, `package.json`, etc.
If Node is missing the Cargo build fails with a pointed message;
there's no feature flag to skip the frontend — it's a required part
of the binary.

## Tests

- **Rust unit** — `crates/dyson/src/controller/http/mod.rs`'s
  `#[cfg(test)] mod tests` block covers content-type dispatch, emoji →
  rating mapping, URL decoding, and shape checks on the embedded
  bundle.
- **Rust integration** — `crates/dyson/tests/http_controller.rs` binds
  the controller to `127.0.0.1:0` and exercises every endpoint with a
  real TCP client.
- **Frontend** — `web/src/__tests__/regression.test.js` runs under
  vitest (`npm test`).  Each case pins a past bug — greyscreen on ⌘4/⌘5
  when nav indices outran view ids, conversations opening at the top,
  control-char placeholders leaking into rendered markdown, etc.
  `npm run build` runs the suite before bundling, so a regression
  fails `cargo build` too.

## Known limits

- **No inbound auth.**  Loopback or Tailscale only.  See top-of-file
  warning.
- **`/api/activity` is empty** until cross-controller
  `BackgroundAgentRegistry` aggregation lands.  Each controller
  currently keeps its own registry.
- **Subagent zoom** doesn't appear in the live UI yet — orchestrator
  tools don't emit structured spawn/complete events for the controller
  to forward.  When the agent calls `security_engineer`, you see one
  fallback tool card for the orchestrator.
- **Model switches don't persist.**  Restarting the controller resets
  the active model to the dyson.json default for every chat.
