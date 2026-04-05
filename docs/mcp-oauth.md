# MCP OAuth 2.0 Authorization

OAuth 2.0 Authorization Code + PKCE for MCP servers that require
interactive authorization (e.g., GitHub Copilot MCP).

**Key files:**
- `src/auth/oauth.rs` — All OAuth logic (PKCE, exchange, refresh, Auth impl, callback server, persistence)
- `src/skill/mcp/mod.rs` — Flow orchestration in `McpSkill::on_load()`
- `src/config/mod.rs` — `McpAuthConfig` type

## Architecture

The OAuth flow is entirely controller-agnostic. It lives in the MCP skill
layer. Controllers never know OAuth exists.

The flow **never blocks the agent**. If no persisted tokens exist,
`on_load()` starts a background callback server, sets the auth URL in the
system prompt, and returns immediately with zero tools. After the user
authorizes, tokens are persisted. The next hot reload loads them.

## Configuration

```json
{
  "mcp_servers": {
    "github-copilot": {
      "url": "https://api.githubcopilot.com/mcp",
      "auth": {
        "type": "oauth",
        "scopes": ["read", "write"]
      }
    }
  }
}
```

All fields except `type` are optional:

| Field | Default | Description |
|-------|---------|-------------|
| `type` | — | Must be `"oauth"` |
| `scopes` | `[]` | OAuth scopes to request |
| `client_id` | (DCR) | Pre-registered client ID |
| `client_secret` | `None` | Supports `SecretValue` resolution |
| `redirect_uri` | `http://127.0.0.1:<random>/callback` | Override redirect URI |
| `authorization_url` | (discovered) | Override authorization endpoint |
| `token_url` | (discovered) | Override token endpoint |
| `registration_url` | (discovered) | Override DCR endpoint |

## Flow

**First use (no tokens):**

1. `on_load()` discovers metadata, registers client (DCR), generates PKCE
2. Starts callback server on `127.0.0.1:<random-port>` in background
3. Registers a temporary `<server>_oauth_submit` tool
4. Sets system prompt with auth URL and instructions
5. Returns immediately — agent not blocked

**Two ways to complete authorization:**

- **Automatic (callback reachable):** User clicks URL, authorizes, browser
  redirects to callback server.  Background task exchanges code, persists
  tokens, touches config → hot reload → tools available.
- **Manual (user behind NAT):** User clicks URL, authorizes, browser tries
  to redirect but can't reach localhost.  User copies the redirect URL from
  their browser and pastes it into the chat.  Agent calls `<server>_oauth_submit`
  with the URL.  Tool extracts the code, exchanges, persists, touches config
  → hot reload → tools available.

Both paths use the same `OAuthPending::complete()` method.  Whichever
fires first wins; the other becomes a no-op.

**Subsequent uses:** `on_load()` loads persisted tokens instantly. No interaction.

**Token refresh:** `OAuth::apply_to_request()` auto-refreshes when expired.

**401 retry:** `HttpTransport` calls `on_unauthorized()` → force refresh → retry once.

## Token Persistence

Stored at `~/.dyson/tokens/<server-name>.json` with `0o600` permissions.
Server names are sanitized to prevent path traversal. All in-memory token
values use `Credential` (zeroize-on-drop).

## Hot Reload

- **Tokens on disk:** Loaded instantly, no interaction.
- **New OAuth server (no tokens):** Background flow starts, agent stays responsive.
- **Config changed:** Persisted tokens loaded; 401 retry handles scope changes.
  Delete the token file and restart for a full re-auth.
- **Server removed:** Old skill dropped cleanly.
