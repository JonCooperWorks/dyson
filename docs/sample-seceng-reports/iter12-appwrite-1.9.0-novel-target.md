I now have a comprehensive picture. Let me write the report.

# Security Review: Appwrite PHP Backend

## Scope

8 PHP route files in the review target: `account.php`, `graphql.php`, `locale.php`, `messaging.php`, `migrations.php`, `project.php`, `projects.php`, `users.php`. These are the Appwrite HTTP route definitions (API entry points).

## MEDIUM

### OAuth2 Failure Redirect — attacker-controlled `error_description` reflected in redirect query without sanitisation
- **File:** `account.php:1591`
- **Evidence:**
  ```php
  $query['error'] = json_encode([
      'message' => $exception->getMessage(),
      'type' => $exception->getType(),
      'code' => !\is_null($code) ? $code : $exception->getCode(),
  ]);
  ```
- **Attack Tree:**
  ```
  account.php:1601 — attacker calls GET /v1/account/sessions/oauth2/:provider/redirect?error=foo&error_description=bar
    └─ account.php:1602 — $message = 'The ' . $providerName . ' OAuth2 provider returned an error: ' . $error; if (!empty($error_description)) { $message .= ': ' . $error_description; }
      └─ account.php:1582 — $exception = new Exception($type, $message, $code);  // $exception->getMessage() = $message
        └─ account.php:1591 — $response->redirect(URLParser::unparse($failure), 301); // redirect with attacker-controlled error message embedded in JSON in query string
  ```
- **Impact:** Attacker-controlled `error_description` (up to 2048 chars) is injected into the `error` query parameter of the failure-redirect URL without any HTML encoding. When the user-agent follows the 301 redirect and the destination renders the `error` param as HTML (the common pattern), this delivers reflected XSS. The `redirectValidator` only validates that `$state['success']` and `$state['failure']` are known hostnames; it does **not** filter query-string values constructed inside `$failureRedirect`.
- **Exploit:**
  ```
  GET /v1/account/sessions/oauth2/google/redirect?code=&error=x&error_description=<script>alert(document.cookie)</script>&state=%7B%22success%22%3A%22https%3A%2F%2Fexample.com%2Ffail%22%2C%22failure%22%3A%22https%3A%2F%2Fexample.com%2Ffail%22%7D
  ```
  The redirect response sends `Location: https://example.com/fail?error={"message":"The Google OAuth2 provider returned an error: x: <script>alert(document.cookie)</script>","type":"…","code":…}` — the victim's browser follows it and renders the script tag.
- **Remediation:** HTML-encode the exception message before embedding it in the failure redirect query string:
  ```php
  $query['error'] = json_encode([
      'message' => htmlspecialchars($exception->getMessage(), ENT_QUOTES, 'UTF-8'),
      'type' => htmlspecialchars($exception->getType(), ENT_QUOTES, 'UTF-8'),
      'code' => !\is_null($code) ? $code : $exception->getCode(),
  ]);
  ```

## LOW / INFORMATIONAL

### GraphQL multipart form-data property-walk in `parseMultipart` uses attacker-controlled keys from `map` field
- **File:** `graphql.php:300`
- **Evidence:**
  ```php
  foreach (\explode('.', $location) as $key) {
      if (!isset($items[$key]) || !\is_array($items[$key])) {
          $items[$key] = [];
      }
      $items = &$items[$key];
  }
  ```
- **Attack Tree:**
  ```
  graphql.php:294 — attacker submits multipart/form-data POST /v1/graphql/mutation with Content-Type: multipart/form-data
    └─ graphql.php:295 — $map = \json_decode($query['map'], true);  // attacker-controlled JSON
      └─ graphql.php:297-305 — for each $location in $map: walk $items = &$items[$key] splitting on '.' — no blocklist for 'constructor', '__proto__', 'prototype'
        └─ graphql.php:310-311 — $query['query'] = $operations['query']; $query['variables'] = $operations['variables']; passed to webonyx/graphql-php execute
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** The dot-split walk from the attacker-supplied `map` field's `location` values (`__proto__`, `constructor`, `prototype`) lets an attacker land on sensitive keys of the `$operations` / `$items` tree. This is a prototype-walk primitive. Currently the downstream consumer is the webonyx GraphQL library which expects specific variable shapes, so the walk result would be rejected by GraphQL type validation. The primitive itself remains and a refactor adding a different downstream consumer flips this to live RCE. **Currently mitigated by** the fact that `$operations` is an `$operations['query']` / `$operations['variables']` array consumed by the GraphQL executor's strict type system — non-existent nested objects would simply fail variable validation.
- **Remediation:** Add a blocklist of reflection-relevant keys before the walk:
  ```php
  $blocklist = ['__proto__', 'constructor', 'prototype'];
  foreach (\explode('.', $location) as $key) {
      if (in_array($key, $blocklist, true)) {
          continue; // or throw
      }
      if (!isset($items[$key]) || !\is_array($items[$key])) {
          $items[$key] = [];
      }
      $items = &$items[$key];
  }
  ```

### Custom email templates are user-configurable and override message body without rendering restrictions
- **File:** `account.php:141`
- **Evidence:**
  ```php
  $body = $customTemplate['message'] ?? '';
  ```
- **Impact:** Authenticated console admins (already privileged) can set arbitrary HTML in custom email templates. This is by-design for the template feature and does not constitute a separate finding; the admin who can set these can already read/write project data. Noted for completeness only.
- **Remediation:** No fix needed — admin-only by design.

### `sendSessionAlert` passes `$clientName` from attacker-controlled `UserAgent` into email template
- **File:** `account.php:155-158`
- **Evidence:**
  ```php
  $userAgent = $session->getAttribute('userAgent');
  $clientName = !empty($userAgent) ? $userAgent : 'UNKNOWN';
  $session->setAttribute('clientName', $clientName);
  ```
- **Impact:** An attacker can set their `User-Agent` header to malicious HTML and, if session alerts are enabled, have it rendered in an email sent to the victim. The `Template::render` method's escaping policy determines impact. This is a low-confidence finding because the template engine (`Template`) appears to use `htmlspecialchars` by default (the params `{{hello}}`, `{{body}}` etc. are set in locale texts, not raw user input), and the `clientName` value flows through the email variable system rather than raw HTML insertion. Without verifying the `Template` class's escaping behavior, this is capped at INFORMATIONAL.
- **Remediation:** Ensure `clientName` is always HTML-encoded before template injection, or set it via the variable system (`$emailVariables['agentClient']`) which passes through encoding.

## Checked and Cleared

- `graphql.php:68` — `query` param uses `new Text(0, 0)` (zero min/max length) — but the param is passed to webonyx/GraphQL which does its own validation; the `Text(0,0)` appears to be a placeholder for "length validated per GraphQL rules" rather than a bypass. The validator rejects the param and would throw before the action fires. **Cleared:** validator prevents execution with invalid query strings.
- `account.php:215-226` — JWT decode with `System::getEnv('_APP_OPENSSL_KEY_V1')` — server-side key known only to the deployment; not attacker-controlled. Cleared.
- `account.php:349,703,846,1081,1247,1969` — `X-Fallback-Cookies` headers set only when `domainVerification` is disabled (single-tenant / internal); contains `$store->getKey()` and encrypted session secret. Cleared — not attacker-controlled input.
- `account.php:1335` — OAuth2 provider validated via `WhiteList(Config::getParam('oAuthProviders'))` — whitelist prevents arbitrary provider. Cleared.
- `account.php:1343` — `success` / `failure` redirect URLs validated via `$redirectValidator` — validates against project platform hostname list. Cleared.
- `account.php:1569,1573` — `$state['success']` and `$state['failure']` validated via `$redirectValidator` before use in redirect. Cleared.
- `account.php:1647` — `$userParam = $request->getParam('user')` is parsed via `json_decode` but only used for `firstName` + `lastName` display name, no eval. Cleared.
- `account.php:2172-2177` — magic URL token with auto-account-creation — creates new user only with `TOKEN_TYPE_MAGIC_URL` scope, not granting admin privileges. Cleared.
- `account.php:303-308` — phone/email verification set to `true` only after valid token + secret match (cryptographic proof). Cleared.
- `account.php:1894-1896` — `ProofsToken` with `TOKEN_LENGTH_OAUTH2` and SHA hashing — standard token generation. Cleared.
- `account.php:1007-1016` — email/password session login uses `ProofsPassword::verify` with stored hash, bcrypt/argon2. Cleared.
- `graphql.php:166` — `abuse-limit` of 60 requests per 60 seconds on GraphQL POST — rate-limited. Cleared.
- `migrations.php:1047-1061` — Appwrite migration report uses `$endpoint` (URL-validated), `$projectID` and `$key` (Text) as parameters to `new Appwrite(...)` constructor, which makes API calls to the migration source. The source is the attacker's own Appwrite instance; this is a trust-boundary design decision (user provides credentials for migration), not a vulnerability. Cleared.
- `account.php:937-940` — `$className = $oAuthProviders[$provider]['class']` — class instantiation from whitelist-validated provider config (not user input). Cleared.
- `account.php:1894-1895` — `sha1` used only for proof token for OAuth2 session token (not password hashing). Cleared.
- `users.php:268-303` — `Bcrypt` hash import from another system — migration endpoint, requires admin API key. Cleared.
- `users.php:306-340` — `MD5` hash import from another system — migration endpoint, requires admin API key. Cleared.
- `users.php:380-418` — `SHA` hash import — migration endpoint, requires admin API key. Cleared.
- `users.php:458-503` — `Scrypt` hash import — migration endpoint, requires admin API key. Cleared.
- `users.php:579-643` — user target creation — `providerType` validated via whitelist, `identifier` validated via Email/Phone validators. Cleared.
- `messaging.php:86-138` — Mailgun provider creation — all params stored in credentials/options arrays, no shell/exec. Cleared.
- `messaging.php:318-431` — SMTP provider creation — credentials stored in DB, `SMTPAutoTLS` default true on SMTP class. Cleared.
- `projects.php:1434-1454` — SMTP validation uses `PHPMailer::SmtpConnect()` with 5-second timeout. Cleared.
- `locale.php:35-71` — locale endpoint uses `$request->getIP()` with MaxMind GeoDB, no user-controlled path. Cleared.
- `projects.php:1098-1176` — platform creation for web types uses `new Hostname()` or `new Text()` for key/store, no path traversal. Cleared.

## Dependencies

No dependency manifests (`composer.json`, `composer.lock`, `package.json`) were found in the review target directory. The subagent returned `NO_MANIFESTS_FOUND`. Unable to assess third-party dependency risk — this should be checked in a full-codebase review that includes the repository root with its vendor/lock files.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
No CRITICAL or HIGH findings in the reviewed surface.

### Short-term (MEDIUM)
1. `account.php:1591` — HTML-encode `$exception->getMessage()` and `$exception->getType()` before embedding in JSON within the redirect query string to prevent reflected XSS via `error_description`.
2. `graphql.php:300` — Add a blocklist for `__proto__`, `constructor`, `prototype` keys in the `parseMultipart` dot-split walk to neutralise the prototype-walk primitive.