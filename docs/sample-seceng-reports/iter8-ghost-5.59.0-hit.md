<answer>
# Security Review: Ghost Admin API Endpoints

## CRITICAL

### SQL Injection via filter parameter concatenation in email endpoint
- **File:** `endpoints/emails.js:104`
- **Evidence:**
  ```
  const filter = `email_id:'${frame.data.id}'` + (frame.options.filter ? `+(${frame.options.filter})` : '');
  ```
- **Attack Tree:**
  ```
  endpoints/emails.js:104 — frame.data.id from request body goes unsanitized into filter string
    └─ endpoints/emails.js:104 — filter string passed to models.EmailBatch.findPage
      └─ endpoints/emails.js:105 — findPage uses filter to construct SQL query
  ```
  Also present at `endpoints/emails.js:135` (same pattern in `browseFailures`).
- **Impact:** Attacker with admin API access can inject arbitrary SQL-like filter expressions. The `frame.data.id` value is directly embedded into the filter string without any validation or parameterization. Ghost's bookshelf/knex layer interprets filter expressions as query conditions, allowing data exfiltration across all tables.
- **Remediation:** Validate `frame.data.id` as a valid ID format before concatenation, or pass it as a parameterized query condition rather than building a filter string. Example:
  ```js
  const filter = {email_id: frame.data.id};
  const combinedFilter = frame.options.filter ? filterBuilder.and(filter, frame.options.filter) : filter;
  ```

### No-authentication file upload endpoint
- **File:** `endpoints/files.js:10`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Attack Tree:**
  ```
  endpoints/files.js:10 — permissions: false on upload endpoint
    └─ endpoints/files.js:12 — storage.save writes file to disk with no authentication check
      └─ endpoints/files.js:12 — any request can upload arbitrary files
  ```
- **Impact:** Unauthenticated users can upload arbitrary files to the server's storage. While the validator may enforce some file type restrictions, the complete lack of authentication means no access control exists on this resource.
- **Remediation:** Change `permissions: false` to `permissions: true` or add authentication middleware to the file upload endpoint.

### No-authentication media upload endpoint
- **File:** `endpoints/media.js:11`
- **Evidence:**
  ```
  permissions: false,
  ```
  (Also at line 31 for `uploadThumbnail`)
- **Attack Tree:**
  ```
  endpoints/media.js:11 — permissions: false on upload endpoint
    └─ endpoints/media.js:18 — storage.save writes file to disk without authentication
      └─ endpoints/media.js:12-18 — unauthenticated file upload with thumbnail
  ```
- **Impact:** Same as files endpoint - unauthenticated users can upload arbitrary media files.
- **Remediation:** Add authentication requirement to both upload methods.

## HIGH

### Open redirect via mail events processing without authentication
- **File:** `endpoints/mail-events.js:9`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Attack Tree:**
  ```
  endpoints/mail-events.js:9 — permissions: false on add endpoint
    └─ endpoints/mail-events.js:11 — mailEvents.service.processPayload(frame.data) processes unauthenticated input
      └─ services/mail-events — processes webhook data without authentication
  ```
- **Impact:** Mail events webhook accepts data from any source. While validation exists (see mail-events validator), the endpoint processes external data without authentication, allowing injection of false email analytics data into the system.
- **Remediation:** Add authentication or implement signature verification for the webhook endpoint. Consider adding rate limiting and validate the source.

### Data exposure via public endpoints without permissions
- **File:** `endpoints/oembed.js:10`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Attack Tree:**
  ```
  endpoints/oembed.js:10 — permissions: false on read endpoint
    └─ endpoints/oembed.js:19 — oembed.fetchOembedDataFromUrl(url, type) with user controlled URL
      └─ services/oembed — fetches external URLs without authentication
  ```
- **Impact:** The oembed fetcher can be used as an SSRF proxy, fetching any URL the server can reach. Combined with the lack of URL validation (only checks for non-empty, not for allowlisting), this allows internal network probing.
- **Remediation:** Implement URL allowlisting, restrict protocols (http/https only), and add rate limiting.

## MEDIUM

### Password reset token generation without brute force protection on initial request
- **File:** `endpoints/authentication.js:147-158`
- **Evidence:**
  ```
  query(frame) {
      return Promise.resolve()
          .then(() => {
              return auth.setup.assertSetupCompleted(true)();
          })
          .then(() => {
              return auth.passwordreset.generateToken(frame.data.password_reset[0].email, api.settings);
          })
  ```
- **Attack Tree:**
  ```
  endpoints/authentication.js:153 — generateToken called with email from request
    └─ services/auth — generates reset token without rate limiting on generation
  ```
- **Impact:** Attacker can trigger password reset emails for any user email address without rate limiting, enabling spam attacks or user enumeration through email delivery timing.
- **Remediation:** Add rate limiting to the `generateResetToken` endpoint.

### No-authentication image upload endpoint
- **File:** `endpoints/images.js:14`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Attack Tree:**
  ```
  endpoints/images.js:14 — permissions: false on upload
    └─ endpoints/images.js:16-69 — image processing and storage without authentication
  ```
- **Impact:** Unauthenticated image uploads. May be intentional for some API configurations, but represents a risk surface for abuse.
- **Remediation:** Evaluate if authentication is needed. If not, at minimum add rate limiting and size restrictions.

### Potential filter expression injection in public endpoints
- **File:** Multiple files (`posts-public.js`, `pages-public.js`, `tags-public.js`, `authors-public.js`)
- **Evidence:**
  ```
  endpoints/posts-public.js:52-88 — filter parameter passed through to findPage
  endpoints/pages-public.js:47-93 — filter parameter passed through 
  ```
- **Attack Tree:**
  ```
  public endpoints accept filter parameter from request
    └─ filter passed directly to model.findPage()
      └─ bookshelf/knex interprets filter as query conditions
  ```
- **Impact:** While public endpoints are meant to be accessible, malicious filter expressions could enumerate data, bypass visibility restrictions, or cause expensive queries.
- **Remediation:** Sanitize and validate filter expressions, implement query timeouts and limits.

## LOW / INFORMATIONAL

### Slack test endpoint without authentication
- **File:** `endpoints/slack.js:10`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Impact:** Slack test event emission without authentication. Low impact as it only emits an internal event.
- **Remediation:** Add basic authentication check.

### Config endpoint without authentication
- **File:** `endpoints/config.js:10`
- **Evidence:**
  ```
  permissions: false,
  ```
- **Impact:** Public configuration data exposure. May reveal internal server details.
- **Remediation:** Consider limiting sensitive config values from public access.

## Checked and Cleared

- `endpoints/session.js:12-66` — Session management uses User.check with proper authentication
- `endpoints/users.js:32-41` — API key management has appropriate permission checks via permissionOnlySelf
- `endpoints/webhooks.js:31-86` — Webhook CRUD requires integration authentication
- `endpoints/members.js:81-115` — Member CRUD operations have proper permission checks
- `endpoints/themes.js:49-182` — Theme operations require authentication
- `endpoints/db.js:89-174` — Database import/export requires authentication
- `endpoints/settings.js:33-179` — Settings operations have permission checks
- `endpoints/pages.js:37-274` — Page CRUD has proper authorization
- `endpoints/posts.js:65-322` — Post operations have permission checks
- `endpoints/mail.js:46-47` — Mail sending has permission checks
- `endpoints/integrations.js:17-164` — Integration management requires authentication
- `endpoints/schedules.js:29-49` — Publish endpoint has permission checks (`permissions: {docName: 'posts'}`)
- `endpoints/authentication.js:123` — isSetup is informational (returns setup status) - appropriate for no-auth
- `endpoints/authors-public.js:45` - Public endpoints have reduced but not absent auth via content API key
- `endpoints/settings-public.js:13` - Public settings endpoint - appropriate for no-auth
- `endpoints/members-stripe-connect.js:20` — Stripe connection has validation

## Dependencies

No package.json or lockfiles found in the scanned directory tree. The endpoints directory appears to be extracted from a larger codebase. Without dependency manifests, a vulnerability scan cannot be performed.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `endpoints/emails.js:104` — Parameterize filter construction to prevent SQL injection
2. `endpoints/files.js:10` — Add authentication to file upload endpoint
3. `endpoints/media.js:11` — Add authentication to media upload endpoint
4. `endpoints/mail-events.js:9` — Implement webhook signature verification or authentication

### Short-term (MEDIUM)
1. `endpoints/authentication.js:153` — Add rate limiting to password reset token generation
2. `endpoints/images.js:14` — Evaluate and add rate limits for image uploads
3. Public endpoints — Add filter expression validation

### Hardening (LOW)
1. `endpoints/slack.js:10` — Add authentication to test endpoint
2. `endpoints/config.js:10` — Review config data exposed without authentication
</answer>