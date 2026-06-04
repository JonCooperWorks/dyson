# Elicitation

MCP is bidirectional. After `initialize`, a connected MCP **server** can turn
around and ask the **client's user** a question mid-tool-call — "which branch?",
"confirm this destructive action", "paste your API key" — via an
`elicitation/create` request. Dyson answers these by parking the request and
surfacing it as a form in the web UI.

This doc covers how that round-trip works end to end. For the wider
bidirectional-MCP picture (sampling, roots, resources, prompts) see
[tool-forwarding-over-mcp.md](tool-forwarding-over-mcp.md).

## The problem it solves

An `elicitation/create` request arrives on the MCP transport's **inbound** path
(`transport.rs`), handled by the per-connection `NotificationRouter`
(`router.rs`). That handler runs on the shared transport and has **no per-chat
handle** — it can't push a prompt to a specific SSE stream or know which user is
"the" user. And the request is synchronous from the server's point of view: the
server's tool call is blocked, waiting for a reply.

So we need to (a) capture the request somewhere process-global, (b) let an
out-of-band surface (the web UI) discover and answer it, and (c) wake the
blocked server with the answer. That's the **elicitation broker**.

## Components

| Piece | File |
|---|---|
| Broker (park / list / resolve) | `crates/dyson/src/skill/mcp/elicitation.rs` |
| Inbound request routing | `crates/dyson/src/skill/mcp/router.rs` (`handle_request`) |
| HTTP bridge | `crates/dyson/src/controller/http/routes/elicitation.rs` |
| UI form + validation | `crates/dyson/src/controller/http/web/src/components/ElicitationModal.jsx` |

## Flow

```
MCP server                Dyson (router + broker)              Web UI (browser)
    │                              │                                  │
    │  elicitation/create ───────▶ │                                  │
    │  { message, requestedSchema }│                                  │
    │                              │ broker.elicit(server, msg, schema)
    │                              │   • seq = next_id++              │
    │                              │   • park in pending{}            │
    │                       (blocks on oneshot)                       │
    │                              │                                  │
    │                              │ ◀───── GET /api/mcp/elicitations │  (poll, 500ms)
    │                              │ ─────▶ { pending: [...] } ──────▶│  render form
    │                              │                                  │
    │                              │ ◀── POST .../<id> {action,content}  submit
    │                              │ broker.resolve(id, result)       │
    │                       (oneshot fires)                           │
    │ ◀── { action, content? } ─── │                                  │
    │  tool call resumes           │                                  │
```

### 1. Capability gating (opt-in)

The client only advertises the `elicitation` capability during `initialize`
**when a UI is present**. The HTTP controller calls
`elicitation::enable_ui()` once at startup; until that flips `UI_ENABLED`, the
router answers any `elicitation/create` with `-32601 Method not found`.

This is load-bearing: a **headless CLI run has no surface to answer a prompt**,
so it must not claim it can — otherwise a server would block forever waiting on
a reply nobody can give.

```rust
"elicitation/create" if super::elicitation::ui_enabled() => { ... }
```

### 2. Parking — `broker.elicit(server, message, schema)`

```rust
pub async fn elicit(&self, server: String, message: String, schema: Value) -> Value
```

- Stamps the request with a monotonic `seq` (`AtomicU64`, seeded at 1) and uses
  it as the string `id`.
- Inserts a `Parked { server, message, schema, seq, responder }` into the
  `Mutex<HashMap>` of pending prompts. `responder` is the sender half of a
  `tokio::sync::oneshot`.
- Awaits the receiver with a **300-second timeout**. On an answer it returns the
  UI's `ElicitResult`; on timeout (or a dropped responder) it cleans up and
  returns `{ "action": "cancel" }` — the spec-safe default.

The broker is **process-global** (`OnceLock`) because elicitation is
process-scoped for a single-user agent.

### 3. Discovery — `GET /api/mcp/elicitations`

`broker.list_pending()` snapshots the open prompts **sorted by `seq`, oldest
first** (HashMap iteration order is otherwise unstable, so the queue would
render unpredictably). Each entry is:

```json
{ "id": "<seq>", "server": "<name>", "message": "...", "requestedSchema": { ... } }
```

The SPA short-polls this endpoint (~500ms) and renders the **first** pending
prompt as a form; if more than one is open it shows a "1 of N" queue indicator.

### 4. The form — `ElicitationModal.jsx`

The modal builds inputs from `requestedSchema.properties` and validates against
the schema **client-side before submit**:

- **Types**: `string`, `number`, `integer`, `boolean`, `enum` (with optional
  `enumNames`).
- **String formats** → HTML5 input types: `email`, `uri`, `date`, `date-time`.
- **Constraints**: `minimum`/`maximum` (numbers), `minLength`/`maxLength`
  (strings), `required` (from `schema.required`).
- **Coercion before the wire**: empty optionals are omitted entirely (never
  sent as `""`), numbers are cast to numbers, booleans to bool.

Keyboard:

- **Esc** → submit `cancel`
- **⌘/Ctrl + Enter** → submit `accept`
- Plain **Enter** is intentionally *not* a submit, so you don't fire the form
  while typing into a field.

### 5. Answering — `POST /api/mcp/elicitations/:id`

Body is the MCP `ElicitResult`: `{ "action": "accept"|"decline"|"cancel",
"content"?: {...} }`. The route:

- caps the body at **64 KiB** (answers are small forms),
- rejects any `action` outside the three valid values with `400` (so a
  malformed result never reaches the waiting server),
- calls `broker.resolve(id, body)`.

```rust
pub async fn resolve(&self, id: &str, result: Value) -> bool
```

`resolve` removes the entry and fires the oneshot. It returns `false` (→ HTTP
`404`) when the id is unknown — already answered, or already timed out and
cleaned up.

### 6. Resume

The oneshot wakes `elicit()`, which returns the `ElicitResult` up through
`handle_request` as the JSON-RPC response to the original `elicitation/create`.
The server's blocked tool call resumes with the user's answer.

## Lifecycle / edge cases

- **No UI (headless)**: capability never advertised; `elicitation/create` →
  `-32601`. Servers that gate on the capability simply won't try.
- **Timeout (5 min)**: parked entry removed, server told `cancel`.
- **Double-answer / stale id**: second `resolve` returns `false` → `404`.
- **Multiple prompts**: queue is strict FIFO by `seq`; the UI answers them one
  at a time, oldest first.

## Tests

- Unit (`elicitation.rs` `mod tests`): `elicit_resolves_with_ui_answer` (park +
  resolve round-trip), `resolve_unknown_id_is_false`,
  `list_pending_is_oldest_first`, `ui_disabled_by_default`.
- Live (`tests/mcp_live.rs`, `#[ignore]`, needs `npx` + network):
  `elicitation_tool_registers_when_client_advertises_elicitation` — asserts that
  after `enable_ui()` the everything server registers its
  `trigger-elicitation-request` tool once it sees the advertised capability.
