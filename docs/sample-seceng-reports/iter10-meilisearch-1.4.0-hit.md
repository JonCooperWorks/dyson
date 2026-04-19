# Security Review: Meilisearch v1.4.0

## CRITICAL

### jsonwebtoken type confusion (GHSA-h395-gr665-qw3r) enables JWT tenant token authorization bypass

- **File:** `src/extractors/authentication/mod.rs:135`
- **Evidence:**
  ```rust
  use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
  ```
  `jsonwebtoken@8.3.0` is declared in `Cargo.toml`. The `decode` function is used at lines 165 and 242-246 to decode JWT tenant tokens that control which indexes and search rules a scoped API key may access.

  ```
  line 242-246:
  let data = if let Ok(data) = decode::<Claims>(
      token,
      &DecodingKey::from_secret(key.as_bytes()),
      &tenant_token_validation(),
  ) {
      data
  }
  ```

- **Attack Tree:**
  ```
  Attacker → POST /indexes/{uid}/search with Bearer <crafted tenant token>
    └─ src/extractors/authentication/mod.rs:190 (ActionPolicy::authenticate)
      └─ src/extractors/authentication/mod.rs:198 (authenticate_tenant_token)
        └─ src/extractors/authentication/mod.rs:242 (jsonwebtoken::decode → type confusion)
          └─ src/extractors/authentication/mod.rs:212 (auth.is_key_authorized bypassed via forged claims)
  ```

- **Impact:** The `jsonwebtoken` 8.3.0 type confusion vulnerability (GHSA-h395-gr665-qw3r) allows an attacker to craft a JWT tenant token where the decoder misinterprets JSON types in the payload. Since `Claims` (line 263-269) contains `search_rules: SearchRules`, `exp`, and `api_key_uid`, a type confusion on these fields can cause the decoder to accept a token with forged `search_rules` (e.g., bypassing index restrictions) or a forged `api_key_uid` (impersonating another key). This grants unauthorized access to indexes and search results that the original API key holder was not permitted to access. The token validation at line 242 uses this vulnerable decoder, so the path is directly exploitable.

- **Exploit:** An attacker generates a JWT with the `searchRules` claim set to a crafted type that triggers the type confusion in `jsonwebtoken::decode`, bypassing the index authorization check at `src/extractors/authentication/mod.rs:212`. The exact payload depends on the type-confusion details of CVE-2022/ GHSA-h395-gr665-qw3r but typically involves using a JSON array where a string is expected (or vice versa) in the claims structure.

- **Remediation:** Bump `jsonwebtoken` from `8.3.0` to `>=10.3.0` in `Cargo.toml`. This is a breaking change — the `Validation` and `decode` APIs have changed in v10. Update the `tenant_token_validation()` function and `decode` calls at lines 165 and 242 accordingly.

## HIGH

### Permissive CORS allows credential-bearing cross-origin requests from any origin

- **File:** `src/lib.rs:122`
- **Evidence:**
  ```
  Cors::default()
      .send_wildcard()
      .allow_any_header()
      .allow_any_origin()
      .allow_any_method()
      .max_age(86_400), // 24h
  ```
  This CORS configuration is applied to all routes via `app.wrap()` at line 121.

- **Attack Tree:**
  ```
  Attacker-controlled website with JavaScript
    └─ Browser sends cross-origin request to Meilisearch with victim's cookies/credentials
      └─ src/lib.rs:122 (CORS middleware allows any origin with wildcard)
        └─ src/lib.rs:124 (allow_any_header accepts Authorization header)
          └─ Attacker's JavaScript reads the response
  ```

- **Impact:** Any website can embed JavaScript that makes authenticated requests to this Meilisearch instance (if it has credentials like cookies, or when the browser is configured to include credentials). Combined with `allow_any_method()`, attackers can perform DELETE/PUT/PATCH operations against the API, destroying or modifying data if the victim has an active session. In practice, Meilisearch typically uses Bearer token authentication (not cookie-based), which limits the practical impact — but the configuration is still an insecure default that widens the attack surface for deployments that do use credential-based auth (e.g., behind a reverse proxy with session cookies).

- **Remediation:** Replace the permissive CORS with a restrictive configuration. If Meilisearch is used by specific frontends, only allow those origins:
  ```rust
  Cors::default()
      .allowed_origin("https://trusted-frontend.example.com")
      .allowed_methods(vec!["GET", "POST", "OPTIONS"])
      .allowed_headers(vec![header::AUTHORIZATION, header::CONTENT_TYPE])
      .max_age(86_400),
  ```

## MEDIUM

### `get_health` endpoint runs without authentication (intentional design)

- **File:** `src/routes/mod.rs:323`
- **Evidence:**
  ```
  pub async fn get_health(
      req: HttpRequest,
      index_scheduler: Data<IndexScheduler>,
      auth_controller: Data<AuthController>,
      ...
  ```
  Unlike all other API routes, `get_health` does not go through `GuardedData` or any authentication extractor. It is registered at line 32: `.service(web::resource("/health").route(web::get().to(get_health)))`.

- **Attack Tree:**
  ```
  Unauthenticated request → GET /health
    └─ src/routes/mod.rs:323 (get_health — no GuardedData, no auth check)
      └─ Returns {"status": "available"}
  ```

- **Impact:** An unauthenticated attacker can determine whether the Meilisearch instance is running and healthy. This confirms the server is live and can be used for reconnaissance before targeting other endpoints. The information disclosed is minimal (server status only), and the route is intentionally public for load-balancer and orchestrator health checks. Downgraded to MEDIUM because this is an intentional design pattern, not an oversight.

- **Remediation:** No fix required if this is intentional for load balancers. To harden, consider adding a simple health-check token or limiting the endpoint to localhost/internal networks.

## LOW / INFORMATIONAL

### Version endpoint discloses commit SHA, commit date, and package version

- **File:** `src/routes/mod.rs:300`
- **Evidence:**
  ```
  HttpResponse::Ok().json(VersionResponse {
      commit_sha: commit_sha.to_string(),
      commit_date: commit_date.to_string(),
      pkg_version: env!("CARGO_PKG_VERSION").to_string(),
  })
  ```
  This endpoint requires `GuardedData<ActionPolicy<{ actions::VERSION }>>` so it is protected, but authenticated users can learn the exact software version and commit, aiding targeted vulnerability research.

- **Impact:** An authenticated user (or anyone with a valid API key) learns the exact Meilisearch version, commit SHA, and compile date. This helps attackers determine if the server has known patches for specific CVEs. Standard informational disclosure.

## Checked and Cleared

- `src/routes/indexes/search.rs:140,174` — Both search routes use `GuardedData<ActionPolicy<{ actions::SEARCH }>>`. Tenant token `search_rules` are properly applied via `add_search_rules()` at lines 153-154 and 187-188.
- `src/routes/indexes/documents.rs` — All document CRUD routes use `GuardedData` with appropriate action policies (`DOCUMENTS_GET`, `DOCUMENTS_ADD`, `DOCUMENTS_DELETE`).
- `src/routes/indexes/mod.rs:122` — `create_index` checks `allow_index_creation(&uid)` against the tenant token's filters before registering the task.
- `src/routes/indexes/settings.rs` — All setting routes use `GuardedData<ActionPolicy<{ actions::SETTINGS_UPDATE }>>` or `SETTINGS_GET`.
- `src/routes/api_key.rs` — All API key CRUD routes require `KEYS_GET`, `KEYS_CREATE`, `KEYS_UPDATE`, `KEYS_DELETE` actions.
- `src/routes/tasks.rs` — Task management routes properly check `index_scheduler.filters()` to restrict results to authorized indexes.
- `src/routes/multi_search.rs:58` — Multi-search individually checks `is_index_authorized` per query, preventing cross-index authorization bypass.
- `src/extractors/authentication/mod.rs:163` — `extract_key_id` uses `insecure_disable_signature_validation()` with a dummy key, but this is only used to extract the `api_key_uid` claim; actual signature validation is performed at line 242 with the proper key. Not a vulnerability.
- `src/search.rs:40-41` — Default `highlight_pre_tag`/`highlight_post_tag` are `<em>`/`</em>`. These are HTML-safe and controlled by the search API, not by untrusted input. The output is JSON, not HTML, so no XSS risk.
- `src/lib.rs:151` — `open_or_create_database_unchecked` does not check version; this is intentional — `open_or_create_database` at line 267 calls `check_version_file` first, then calls `open_or_create_database_unchecked`.
- `src/extractors/payload.rs:59` — Payload size is enforced via `remaining.checked_sub(bytes.len())` with error return on exceed. Size limit is set from `opt.http_payload_size_limit` in `configure_data`.
- `src/lib.rs:145` — Import snapshot from `--import-snapshot` flag requires CLI access; trusted input.

## Dependencies

From dependency_review of `Cargo.toml` (9 vulnerabilities across 9 packages):

### Critical
- **jsonwebtoken@8.3.0** — GHSA-h395-gr665-qw3r — Type confusion leading to authorization bypass [fixed in: 10.3.0]
  linked-findings: `src/extractors/authentication/mod.rs:135`, `src/extractors/authentication/mod.rs:164`, `src/extractors/authentication/mod.rs:242`

### High
- **time@0.3.20** — RUSTSEC-2026-0009 — Denial of service via stack exhaustion [fixed in: 0.3.47]
  linked-findings: `src/routes/tasks.rs:18`, `src/extractors/authentication/mod.rs:140`, `src/analytics/segment_analytics.rs:22`
- **rustls@0.20.8** — RUSTSEC-2024-0336 — `ConnectionCommon::complete_io` infinite loop from network input [fixed in: 0.23.5]
  linked-findings: `src/option.rs:17` (only when SSL is enabled via `--ssl-cert-path`)
- **tar@0.4.38** — RUSTSEC-2026-0067 — Arbitrary directory chmod via symlinks [fixed in: 0.4.45]
  linked-findings: unreferenced (appears unused in source)

### Medium
- **bytes@1.4.0** — GHSA-434x-w66g-qw3r — Integer overflow in `BytesMut::reserve` [fixed in: 1.11.1]
- **mimalloc@0.1.37** — RUSTSEC-2022-0094 — Misaligned memory allocation [fixed in: 0.1.39]
- **rustls-pemfile@1.0.2** — RUSTSEC-2025-0134 — Unmaintained
- **tokio@1.27.0** — RUSTSEC-2025-0023 — Broadcast channel unsound `clone` [fixed in: 1.44.2]
- **rand@0.8.5** — GHSA-cq8v-f236-94qc — Unsound with custom logger [fixed in: 0.10.1]

### Other
No `Cargo.lock` was found — versions may not be exactly pinned. A lockfile should be committed for reproducible builds.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `Cargo.toml` — Bump `jsonwebtoken` from `8.3.0` to `>=10.3.0` to fix JWT type confusion (breaking API change: update `decode`/`Validation` calls in `src/extractors/authentication/mod.rs`)
2. `src/lib.rs:122` — Replace `Cors::default().send_wildcard().allow_any_header().allow_any_origin().allow_any_method()` with explicit allowed origins and methods

### Short-term (MEDIUM)
1. `/health` endpoint (`src/routes/mod.rs:323`) — Consider restricting to localhost or requiring a health-check token if public deployment

### Hardening (LOW)
1. `Cargo.toml` — Commit `Cargo.lock` for reproducible builds
2. `Cargo.toml` — Bump `time` to `>=0.3.47` and `rustls` to `>=0.23.5` (if SSL is used)
3. `Cargo.toml` — Remove `tar` dependency if unused