# MCP OAuth 2.0 Authorization

Dyson supports OAuth 2.0 Authorization Code with PKCE for MCP servers that
require interactive authorization (e.g., GitHub Copilot MCP).

**Key files:**
- `src/auth/oauth.rs` — Pure OAuth functions (discovery, DCR, PKCE, exchange, refresh)
- `src/auth/oauth_credential.rs` — `OAuthAuth` (Auth trait impl) + token persistence
- `src/auth/oauth_callback.rs` — Temporary HTTP callback server
- `src/skill/mcp/mod.rs` — OAuth flow orchestration in `McpSkill`
- `src/config/mod.rs` — `McpAuthConfig` configuration types

---

## Architecture: Controller-Agnostic Design

The OAuth flow lives entirely in the MCP skill layer.  Controllers (Terminal,
Telegram, etc.) never know OAuth exists.

```
Any Controller ←→ Agent ←→ McpSkill ←→ HttpTransport + OAuthAuth
                    ↑                         ↑
           sees auth URL              auto-refreshes tokens
           in system prompt           transparently
```

The auth URL is surfaced through the agent's system prompt.  The agent relays
it to the user through whatever controller is active.  The callback server
runs on the Dyson host independently.

This means:
- Terminal users see the URL in their terminal and click it
- Telegram users receive the URL as a message and tap it
- Any future controller works the same way without any OAuth code

---

## Configuration

Add an `auth` object to any HTTP MCP server in `dyson.json`:

### Minimal (auto-discovery + Dynamic Client Registration)

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

Dyson will:
1. Discover the authorization server via `/.well-known/oauth-authorization-server`
2. Register a client via Dynamic Client Registration (DCR)
3. Run the PKCE flow

### Full (pre-registered client, explicit endpoints)

```json
{
  "mcp_servers": {
    "my-server": {
      "url": "https://mcp.example.com/mcp",
      "auth": {
        "type": "oauth",
        "client_id": "my-registered-client-id",
        "client_secret": { "resolver": "insecure_env", "name": "MY_CLIENT_SECRET" },
        "scopes": ["read", "write"],
        "authorization_url": "https://auth.example.com/authorize",
        "token_url": "https://auth.example.com/token"
      }
    }
  }
}
```

### Config Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `type` | Yes | — | Must be `"oauth"` |
| `scopes` | No | `[]` | OAuth scopes to request |
| `client_id` | No | (DCR) | Pre-registered client ID; if absent, uses DCR |
| `client_secret` | No | `None` | Client secret (supports `SecretValue` resolution) |
| `redirect_uri` | No | `http://127.0.0.1:<random>/callback` | Override redirect URI |
| `authorization_url` | No | (discovered) | Override authorization endpoint |
| `token_url` | No | (discovered) | Override token endpoint |
| `registration_url` | No | (discovered) | Override DCR endpoint |

---

## Flow Walkthrough

### First Use (No Persisted Tokens)

```
McpSkill::on_load()
  ├── Check ~/.dyson/tokens/github-copilot.json → not found
  ├── Discover metadata from /.well-known/oauth-authorization-server
  ├── (Optional) Register client via DCR
  ├── Generate PKCE pair (code_verifier + S256 code_challenge)
  ├── Start callback server on 127.0.0.1:<random-port>
  ├── Build authorization URL
  ├── Store pending auth state
  └── Set system_prompt = "Please visit this URL to authorize: ..."

Agent sees system prompt → tells user to click the URL

User clicks URL → browser → OAuth server → grants access → redirect

Callback server receives GET /callback?code=...&state=...
  ├── Validates state (CSRF protection)
  ├── Returns "Authorization Complete" HTML page
  └── Sends code via oneshot channel

McpSkill::before_turn() (next agent turn)
  ├── Checks oneshot channel (non-blocking)
  ├── Exchanges code for tokens (POST to token endpoint with PKCE verifier)
  ├── Persists tokens to ~/.dyson/tokens/github-copilot.json
  └── Returns prompt indicating auth is complete
```

### Subsequent Uses (Persisted Tokens)

```
McpSkill::on_load()
  ├── Load ~/.dyson/tokens/github-copilot.json → found
  ├── Has refresh token? → create OAuthAuth
  └── Run MCP handshake (initialize, tools/list) immediately
```

No user interaction needed.  If the access token has expired, `OAuthAuth`
refreshes it automatically on the first request.

### Token Refresh (Automatic)

```
HttpTransport::send_request()
  └── auth.apply_to_request(req)
      └── OAuthAuth checks expires_at
          ├── Not expired → add Authorization: Bearer <token>
          └── Expired → refresh_token() → update credential → add header
```

### 401 Retry (Server-Side Rejection)

```
HttpTransport::send_request()
  ├── Send request with current token
  ├── Receive 401 Unauthorized
  ├── auth.on_unauthorized() → force token refresh
  ├── Rebuild request with new token
  └── Retry once
```

This handles clock skew and server-side token revocation.

---

## Token Persistence

Tokens are stored at `~/.dyson/tokens/<server-name>.json`.

### File Format

```json
{
  "access_token": "gho_xxxxxxxxxxxx",
  "refresh_token": "ghr_xxxxxxxxxxxx",
  "expires_at_epoch": 1700000000,
  "token_url": "https://auth.example.com/token",
  "client_id": "my-client-id",
  "client_secret": null
}
```

### Security

- Directory created with `0o700` (owner only) on Unix
- Files created with `0o600` (owner read/write only) on Unix
- Server names are sanitized to prevent path traversal
- All in-memory token values use `Credential` (zeroize-on-drop)

---

## PKCE (Proof Key for Code Exchange)

Dyson uses PKCE (RFC 7636) with the S256 method:

1. Generate 32 random bytes → base64url encode → `code_verifier` (43 chars)
2. SHA-256 hash the verifier → base64url encode → `code_challenge` (43 chars)
3. Send `code_challenge` in the authorization request
4. Send `code_verifier` in the token exchange request

The server verifies that `SHA-256(code_verifier) == code_challenge`, proving
the same client that initiated the flow is exchanging the code.

This prevents authorization code interception attacks and is required for
public clients (no client_secret).

---

## Callback Server

The callback server is a temporary HTTP server that receives the OAuth
redirect after the user authorizes in their browser.

- Binds to `127.0.0.1:0` (random port, loopback only)
- Listens for `GET /callback?code=...&state=...`
- Validates the `state` parameter (CSRF protection)
- Returns an HTML success page to the browser
- Sends the authorization code via a oneshot channel
- Auto-shuts down after 5 minutes or after receiving a callback

### When the Callback Server Isn't Reachable

If Dyson runs behind NAT or in a container where `127.0.0.1` on the host
isn't reachable from the user's browser:

1. Set a custom `redirect_uri` in the config pointing to a reachable address
2. Set up a reverse proxy to forward the callback to Dyson's callback server
3. Or use a publicly accessible `redirect_uri` and configure port forwarding

Future enhancement: manual paste fallback where the user copies the redirect
URL from their browser and pastes it into the chat.

---

## Module Architecture

### `src/auth/oauth.rs` — Pure Functions

Stateless, side-effect-free (except HTTP calls), fully unit-testable:

- `discover_metadata()` — Fetch OAuth server metadata from well-known URL
- `register_client()` — Dynamic Client Registration (RFC 7591)
- `generate_pkce()` — PKCE code_verifier + S256 code_challenge
- `build_auth_url()` — Construct authorization URL with query params
- `exchange_code()` — Exchange authorization code for tokens
- `refresh_token()` — Refresh expired access tokens
- `generate_state()` — Random state parameter for CSRF protection

### `src/auth/oauth_credential.rs` — Auth Trait Implementation

- `OAuthCredential` — Mutable token state (access_token, refresh_token, expires_at)
- `OAuthAuth` — `Auth` trait impl with `Arc<RwLock<OAuthCredential>>`
  - `apply_to_request()` — Auto-refresh on expiry, add Bearer header
  - `on_unauthorized()` — Force-refresh for 401 recovery
- `persist_tokens()` / `load_tokens()` — File I/O for token persistence

### `src/auth/oauth_callback.rs` — Callback Server

- `start_callback_server()` — Start temporary hyper HTTP server
- Returns `(port, JoinHandle, oneshot::Receiver<CallbackResult>)`

### `src/skill/mcp/mod.rs` — Flow Orchestration

- `McpSkill::create_oauth_transport()` — Full OAuth setup
- `McpSkill::before_turn()` — Poll for callback completion
- `OAuthPendingAuth` — State for in-progress OAuth flow

### `src/skill/mcp/transport.rs` — 401 Retry

- `HttpTransport::send_request()` — On 401, calls `auth.on_unauthorized()`,
  rebuilds request, retries once

---

## Error Handling

OAuth errors use the `DysonError::OAuth { server, message }` variant.
This is separate from `DysonError::Mcp` to distinguish auth failures from
MCP protocol errors.

Common error scenarios:
- Discovery failure (server doesn't have well-known endpoint)
- DCR rejection (server doesn't support dynamic registration)
- Token exchange failure (invalid code, expired code, wrong verifier)
- Refresh failure (refresh token revoked or expired)
- Callback timeout (user didn't complete authorization within 5 minutes)
