# Authentication (`auth`)

## Overview
One composable `Auth` trait covers every HTTP auth boundary: adding credentials to outgoing requests (LLM APIs, MCP servers) and validating incoming requests (HTTP controller, loopback MCP server). Implementations cover API keys, plaintext and Argon2id-hashed bearer tokens, static headers, OAuth 2.1 with PKCE for MCP servers, OIDC JWT validation, composition, and an explicit no-auth opt-out.

## Interface
- `Auth` trait: `apply_to_request(...)`, `validate_request(headers) -> Result<AuthInfo>`, `on_unauthorized()` (both directions have defaults) [mod.rs:Auth]
- `AuthInfo` [mod.rs:AuthInfo]; `extract_bearer(headers)` [mod.rs:extract_bearer]
- `ApiKeyAuth` [api_key.rs:ApiKeyAuth], `BearerTokenAuth` (`generate()`, `token()`) [bearer.rs:BearerTokenAuth], `HashedBearerAuth` [hashed_bearer.rs:HashedBearerAuth], `StaticHeadersAuth` [static_headers.rs:StaticHeadersAuth], `CompositeAuth` [composite.rs:CompositeAuth], `DangerousNoAuth` [no_auth.rs:DangerousNoAuth]
- `Credential` - zeroize-on-drop secret wrapper with a redacting `Debug` [credential.rs:Credential]
- `OidcAuth`, `OidcConfig` [oidc.rs:OidcAuth, oidc.rs:OidcConfig]
- OAuth: `OAuth`, `OAuthCredential`, `discover_metadata`, `register_client`, `build_auth_url`, `canonical_resource`, `exchange_code`, `refresh_token`, `start_callback_server`, `persist_tokens`, `load_tokens`, `generate_pkce`, `generate_state` (some re-exported from `dyson_common::oauth`) [oauth.rs:OAuth, oauth.rs:start_callback_server]

## Invariants
- `BearerTokenAuth::generate` uses OS randomness and compares tokens in constant time via `dyson_common::util::ct_eq_bytes` [bearer.rs:BearerTokenAuth]
- `HashedBearerAuth` stores only an Argon2id PHC hash and verifies each request with `verify_password`; it never holds the plaintext [hashed_bearer.rs:HashedBearerAuth]
- OIDC validates signature, `iss` (exact match to discovered issuer), `aud`, `exp`, `nbf`, and required scopes; accepted algorithms default to `[RS256]` [oidc.rs:OidcAuth]
- JWKS refresh is rate-limited to at most once per `MIN_JWKS_REFRESH = 300 s` [oidc.rs:MIN_JWKS_REFRESH]
- OAuth callback server binds `127.0.0.1:0`; `state` older than `STATE_MAX_AGE = 600 s` is rejected [oauth.rs:start_callback_server, oauth.rs:STATE_MAX_AGE]
- `Credential` zeroes memory on drop and redacts in `Debug` [credential.rs:Credential]

## Side Effects
- Outbound HTTP to OAuth/OIDC metadata, registration, token, and JWKS endpoints [oauth.rs:discover_metadata, oauth.rs:exchange_code, oidc.rs:OidcAuth]
- Persists OAuth tokens to `~/.dyson/tokens/<server>.json` (directory `0o700`, file `0o600` on Unix) [oauth.rs:persist_tokens, oauth.rs:load_tokens]
- Opens a temporary loopback HTTP listener for OAuth redirects [oauth.rs:start_callback_server]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
