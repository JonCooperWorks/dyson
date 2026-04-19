# Security Review: PlausibleWeb

Review scope: `plausible_web/` directory — Phoenix web layer for Plausible Analytics. The application uses Ecto with parameterized queries, Phoenix session cookies with signing, and well-structured authorization plugs. After thorough review of controllers, plugs, templates, and the endpoint configuration, **no server-side vulnerabilities were found** in this codebase that pass the Finding Gate.

## CRITICAL

No findings.

## HIGH

No findings.

## MEDIUM

No findings.

## LOW / INFORMATIONAL

### Open redirect via `redirect` query parameter on `/logout`
- **File:** `controllers/auth_controller.ex:556`
- **Evidence:**
  ```elixir
  def logout(conn, params) do
    redirect_to = Map.get(params, "redirect", "/")
    …
    |> redirect(to: redirect_to)
  end
  ```
- **Attack Tree:**
  ```
  attacker → GET /logout?redirect=https://evil.com (unauthenticated) — attacker crafts a malicious link
    └─ auth_controller.ex:556 — `Map.get(params, "redirect", "/")` reads attacker-supplied `redirect` param
      └─ auth_controller.ex:561 — `redirect(to: redirect_to)` sends a 302 to the attacker's domain
  ```
- **Impact:** An attacker can phish by distributing a link like `https://plausible.io/logout?redirect=https://evil.com`. After logout, the user is sent to the attacker's site. This is the open redirect OWASP classification. Impact is limited because it only works on the logout path — an authenticated user must first visit the target site to be logged out.
- **Remediation:** Validate that the `redirect` target is a same-origin path. For example, prefix with `/` and ensure the first character after domain is `/` (relative path), or use an allowlist:
  ```elixir
  def logout(conn, params) do
    redirect_to = Map.get(params, "redirect", "/")
    redirect_to = if URI.parse(redirect_to).host == nil, do: URI.parse(redirect_to).path || "/", else: "/"
    conn
    |> configure_session(drop: true)
    |> delete_resp_cookie("logged_in")
    |> redirect(to: redirect_to)
  end
  ```

## Checked and Cleared

- `plugs/auth_plug.ex:10` — Session-based user lookup uses parameterized Ecto query; no SQL injection.
- `plugs/authorize_site_access.ex:8` — Site authorization validates membership or public/shared-link access before proceeding; returns 404 if unauthorized.
- `plugs/authorize_stats_api.ex:12` — API key auth validates bearer token, hashes key, checks site ownership via parameterized queries.
- `plugs/authorize_sites_api.ex:11` — API key auth for site provisioning checks `sites:provision:*` scope guard.
- `plugs/crm_auth_plug.ex:9` — Requires super_admin session before exposing CRM/admin pages.
- `plugs/tracker.ex:33` — Tracker plug serves only from a compile-time allowlist of JS filenames; no path traversal.
- `plugs/favicon.ex:82` — Favicon proxy: source is URL-decoded and forwarded only to `icons.duckduckgo.com/ip3/`; CSP `script-src 'none'` and `Content-Disposition: attachment` prevent SVG XSS.
- `endpoint.ex:46` — Session cookie uses `SameSite=Lax`, `HttpOnly` implicit (Plug.Session default), signing salt set (not secret by itself — derives from `secret_key_base`).
- `endpoint.ex:36` — JSON parser uses Phoenix.json_library (Jason); no deserialization sink for attacker data.
- `controllers/auth_controller.ex:63` — Registration uses Ecto changeset; no mass-assignment (changeset defines explicit permitted fields).
- `controllers/auth_controller.ex:203` — Email activation uses numeric `code` validated by `Integer.parse`; no code injection.
- `controllers/auth_controller.ex:265` — Password reset: timing-safe, identical response for valid/invalid email (user enumeration mitigated), captcha required.
- `controllers/auth_controller.ex:321` — Password reset confirms token with `Auth.Token.verify_password_reset` before applying.
- `controllers/auth_controller.ex:360` — Login uses per-IP and per-user rate limits (Hammer), dummy password hash on wrong email, generic error message.
- `controllers/auth_controller.ex:516` — API key creation: generates key with `:crypto.strong_rand_bytes`, stores via changeset.
- `controllers/auth_controller.ex:535` — API key deletion: parameterized query restricts to `user_id == ^conn.assigns[:current_user].id`.
- `controllers/auth_controller.ex:597` — Google OAuth callback: decodes state with `Jason.decode!`, validates site ownership before storing tokens.
- `controllers/api/external_controller.ex:13` — Event ingestion endpoint: no auth required (by design for analytics tracking), validates via `Ingestion.Request.build` + Ecto changeset.
- `controllers/api/paddle_controller.ex:6` — Paddle webhook verifies RSA signature before processing any alert handler.
- `controllers/api/external_sites_controller.ex:9` — Site CRUD via API: gated by `AuthorizeSitesApiPlug` requiring `sites:provision:*` scope + API key.
- `controllers/api/external_sites_controller.ex:159` — `serialize_errors`: only serializes Ecto changeset field names (known atoms) — not attacker-controlled.
- `controllers/api/stats_controller.ex:94` — Stats API queries use Ecto parameterized queries (`Query.from`, `Filters.add_prefix`).
- `controllers/api/external_stats_controller.ex:118` — Metric parsing: metrics validated against hardcoded allowlists (`@event_metrics`, `@session_metrics`) before `String.to_atom/1`; no atom exhaustion.
- `controllers/stats_controller.ex:95` — CSV export: requires `AuthorizeSiteAccess`, uses parameterized queries.
- `controllers/stats_controller.ex:258` — Shared link auth: `Plausible.Auth.Password.match?` for password comparison, token-based cookie for 24h access.
- `remote_ip.ex:2` — IP extraction trusts Cloudflare `cf-connecting-ip` header first; no header injection sink.
- `captcha.ex:14` — hCaptcha verification delegates to hCaptcha service; returns true if captcha is disabled (config).
- `email.ex` — All email templates use Bamboo `render` with Bamboo.Phoenix (HTML-escapes by default in EEx templates); no `raw()` calls in `email/` templates.
- `router.ex:21` — `:csrf` pipeline applies `protect_from_forgery` to all browser POST/PUT/DELETE routes.
- `router.ex:37` — Public API pipeline (`:public_api`) has no auth but each endpoint requires API key verification in the specific plug (`AuthorizeStatsApiPlug`, `AuthorizeSitesApiPlug`).

## Dependencies

No `mix.exs` or lockfile was present in the review scope (only `plausible_web/` directory). The dependency scan found no manifests to analyze. No vulnerable dependencies can be assessed from the files in scope.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
No findings requiring immediate remediation.

### Short-term (MEDIUM)
No findings.

### Hardening (LOW)
1. `controllers/auth_controller.ex:556` — Validate `redirect` param on `/logout` to same-origin paths only to prevent open redirect phishing.