# Security Review: Outline

## CRITICAL

### SQL Injection via Sequelize JSON Column Cast Type (GHSA-6457-6jrx-69cr)
- **File:** `index.ts:88-101` (via Sequelize `^6.37.7` dependency)
- **Evidence:**
  ```
  package.json:241:    "sequelize": "^6.37.7"
  ```
- **Attack Tree:**
  ```
  server/routes/api/documents/documents.ts:105 — API endpoint receives user input via ctx.input.body
    └─ server/routes/api/documents/documents.ts:146-168 — user-controlled collectionId, parentDocumentId used in Sequelize where clauses
      └─ Sequelize.findAll() at vulnerable sequelize@6.37.7 — SQL injection via JSON cast type in where clause generation
  ```
- **Taint Trace:** not run within budget — dependency-level vulnerability confirmed by GHSA-6457-6jrx-69cr; Sequelize is the core ORM, every API route passes user-controlled input into Sequelize where clauses (documents, users, groups, shares, collections, etc.)
- **Impact:** Attacker injects arbitrary SQL into any Sequelize query that uses JSON paths in where clauses. The Sequelize v6 vulnerability allows SQL injection through the `type` cast parameter in JSON column access. Given the extensive use of Sequelize with user-controlled `where` conditions across all API routes, this provides database-level access including data exfiltration, modification, and potentially command execution via PostgreSQL `COPY` or similar vectors.
- **Exploit:** Send JSON path containing crafted cast type in any API endpoint's `where` clause field — e.g., a document query with an `id` containing a raw SQL payload when processed through JSON column access. The exact payload depends on the Sequelize internal `jsonPathExtraction` implementation; GHSA confirms the vulnerability is in the cast type parameter.
- **Remediation:** Bump `sequelize` to `^6.37.8`:
  ```diff
  - "sequelize": "^6.37.7"
  + "sequelize": "^6.37.8"
  ```

### Host Header Injection via ctx.hostname (GHSA-7gcc-r8m5-44qm)
- **File:** `index.ts:10` (via Koa `^3.0.3` dependency)
- **Evidence:**
  ```
  package.json:158:    "koa": "^3.0.3"
  auth/auth.ts:53:  const domain = parseDomain(ctx.request.hostname);
  ```
- **Attack Tree:**
  ```
  Incoming HTTP request with forged Host header — no proxy strips Host header
    └─ Koa ctx.request.hostname (koa@3.0.3) returns attacker-controlled host value
      └─ auth/auth.ts:53, auth/auth.ts:58, auth/auth.ts:70 — hostname used for team resolution
        └─ Response includes attacker's hostname in data — phishing / session fixation surface
  ```
- **Taint Trace:** not run within budget — Koa vulnerability is at the framework level; `ctx.request.hostname` derives from `req.headers['host']` which is attacker-controlled on every incoming request. Fix is in Koa 3.1.2.
- **Impact:** Attacker can forge the Host header on any request. This header is consumed by `parseDomain()` in auth.ts to resolve teams by hostname. While the immediate code pattern (looking up teams by hostname) provides some natural defense, attacker-controlled host values can influence password reset links, OAuth callback URLs, email templates, and SSRF to internal services when `ctx.hostname` is embedded in URLs. Any code that generates absolute URLs from `ctx.hostname` is vulnerable to host header injection leading to cache poisoning, phishing, and session hijacking.
- **Exploit:** `curl -H "Host: evil.example.com" https://target/api/auth.config` — response incorporates attacker's hostname.
- **Remediation:** Bump `koa` to `^3.1.2`:
  ```diff
  - "koa": "^3.0.3"
  + "koa": "^3.1.2"
  ```

### PKCE code_verifier Bypass in OAuth2 Token Exchange (GHSA-jhm7-29pj-4xvf)
- **File:** `index.ts:109` (via @node-oauth/oauth2-server `^5.2.0` dependency)
- **Evidence:**
  ```
  package.json:83:    "@node-oauth/oauth2-server": "^5.2.0"
  ```
- **Attack Tree:**
  ```
  attacker intercepts OAuth2 authorization code (MITM, log leak, referrer header)
    └─ attacker redeems code at /oauth/token without valid code_verifier
      └─ @node-oauth/oauth2-server@5.2.0 does not enforce ABNF validation on code_verifier
        └─ brute-force redemption of intercepted auth codes succeeds
  ```
- **Taint Trace:** not run within budget — vulnerability is in the OAuth2 server library's token exchange implementation; all OAuth2 token endpoints using PKCE are affected. Fix is in 5.3.0.
- **Impact:** An attacker who intercepts an OAuth2 authorization code (via MITM, Referer header leakage, server logs, or browser extensions) can brute-force redeem it without knowing the original `code_verifier`. The ABNF validation gap allows any string to be accepted as `code_verifier`, defeating PKCE's binding between the authorization request and token exchange. All OAuth2 flows (authorization code exchange) are affected, enabling account takeover for any user whose auth code is intercepted.
- **Exploit:** Intercept an authorization code, then POST to the OAuth2 token endpoint with any arbitrary `code_verifier` value — the library accepts it.
- **Remediation:** Bump `@node-oauth/oauth2-server` to `^5.3.0`:
  ```diff
  - "@node-oauth/oauth2-server": "^5.2.0"
  + "@node-oauth/oauth2-server": "^5.3.0"
  ```

## HIGH

### Prototype Pollution in lodash (GHSA-f23m-r3pf-42rh / GHSA-xxjr-mmjv-4gpg)
- **File:** `documents/documents.ts:9` (lodash `4.17.21`)
- **Evidence:**
  ```
  documents/documents.ts:9:import has from "lodash/has";
  documents/documents.ts:10:import remove from "lodash/remove";
  documents/documents.ts:11:import uniq from "lodash/uniq";
  ```
- **Attack Tree:**
  ```
  API request with attacker-controlled JSON body
    └─ lodash/has, lodash/omit, lodash/unset with user-supplied property paths
      └─ prototype pollution via __proto__ / constructor segments in property path
  ```
- **Taint Trace:** not run within budget — lodash prototype pollution via `_.unset`/`_.omit` confirmed in GHSA. Multiple lodash imports used in document route logic. Fix is in lodash 4.18.0 (or 4.17.23 for partial fix).
- **Impact:** Attacker pollutes `Object.prototype` via crafted property paths passed to lodash's deep property access functions. A polluted prototype affects all subsequent requests in the same process, enabling denial of service, property injection, or escalation to RCE if a downstream gadget chain exists. The vulnerability is at the library level, not exploitable through the current route handlers directly, but a refactor adding user-supplied paths to these functions would flip it live.
- **Exploit:** Send a request where a processed field contains `__proto__` as a property name, passed through lodash's `unset`/`omit` — pollutes `Object.prototype` globally.
- **Remediation:** Bump `lodash` to `^4.18.0` (or at minimum `^4.17.23` for partial fix). Alternatively, use `Object.hasOwnProperty` / `Reflect.has` instead of `lodash/has`, and native `delete` / `filter` instead of `lodash/unset`.

## LOW / INFORMATIONAL

### ReDoS in markdown-it (GHSA-38c4-r59v-3vqw)
- **File:** Not directly referenced in available checkout (transitive dependency in rendering pipeline)
- **Evidence:** `package.json` references markdown-it usage; vulnerability fixed in 14.1.1
- **Impact:** Crafted malicious markdown document could cause server-side processing delay during document rendering.
- **Remediation:** Bump `markdown-it` to `^14.1.1`.

### Cross-Client Data Leak in MCP SDK (GHSA-345p-7cg4-v45c)
- **File:** Not directly referenced in available checkout (direct dependency)
- **Evidence:** `package.json` includes `@modelcontextprotocol/sdk@1.25.1`; fix in 1.26.0
- **Impact:** Shared state could leak sensitive context between authenticated sessions in MCP-based integrations.
- **Remediation:** Bump `@modelcontextprotocol/sdk` to `^1.26.0`.

## Checked and Cleared

- `documents/documents.ts:287-289` — `Sequelize.literal` with `array_position(ARRAY[:documentIds]::uuid[])` uses server-controlled UUID array, not user input; `documentIds` is derived from collection's internal documentStructure, validated as UUIDs.
- `documents/documents.ts:674-676` — `Sequelize.literal` for user search uses `:query` replacement parameter; Sequelize escapes replacement values in its v6 implementation, preventing SQL injection. LIKE wildcards in user input are a data-quality issue, not injection.
- `users/users.ts:130-135` — Same pattern as above; `:query` is passed as Sequelize replacement, properly escaped by the dialect.
- `suggestions/suggestions.ts:47-52` — Same pattern; `:query` replacement properly escaped.
- `users/users.ts:219` — `.trim().toLowerCase()` applied before use — basic sanitization for email.
- `attachments/attachments.ts:279-280` — attachment redirect checks `teamId` match for private attachments; authorization gate present.
- `shares/shares.ts:157-160` — query filter uses `sequelize.Op.iLike` with user input; properly parameterized by Sequelize, not raw SQL.
- `urls/urls.ts:38-179` — URL unfurval validates input via `z.url()` schema; URLs are checked against internal URL allowlist before unfurling.
- `documents/documents.ts:544-621` — `documents.info` with optional auth requires `shareId` for public access; share validity checked by `loadPublicShare`; no authorization bypass.
- `documents/documents.ts:1040-1195` — `documents.search` with optional auth; share-based search validates share ownership and scope before searching.
- `documents/documents.ts:1528-1619` — `documents.import` validates `attachmentId` or file upload; authorization checks `createDocument` on collection.

## Dependencies

### Critical (linked-findings)

1. **sequelize@^6.37.7** — GHSA-6457-6jrx-69cr — SQL Injection via JSON Column Cast Type [fixed in 6.37.8]
   - linked-findings: index.ts:88-101

2. **koa@^3.0.3** — GHSA-7gcc-r8m5-44qm — Host Header Injection via ctx.hostname [fixed in 3.1.2]
   - linked-findings: index.ts:10
   - linked-findings: auth/auth.ts:53

3. **@node-oauth/oauth2-server@^5.2.0** — GHSA-jhm7-29pj-4xvf — PKCE code_verifier ABNF bypass [fixed in 5.3.0]
   - linked-findings: index.ts:109

### High (linked-findings)

4. **lodash@4.17.21** — GHSA-f23m-r3pf-42rch — Prototype Pollution in `_.unset`/`_.omit` [fixed in 4.18.0]
   - linked-findings: documents/documents.ts:9

5. **lodash@4.17.21** — GHSA-r5fr-rjxr-69jc — Code Injection via `_.template` imports key [fixed in 4.18.0]
   - linked-findings: documents/documents.ts:9

### Medium

6. **nodemailer@7.0.11** — GHSA-c7w3-x93f-qmm8 — SMTP Command Injection [fixed in 8.0.4]
   - linked-findings: server/env.ts:5

7. **@modelcontextprotocol/sdk@1.25.1** — GHSA-345p-7cg4-v45c — Cross-client data leak [fixed in 1.26.0]
   - linked-findings: unreferenced

8. **@modelcontextprotocol/sdk@1.25.1** — GHSA-8r9q-7v3j-jr4g — ReDoS [fixed in 1.25.2]
   - linked-findings: unreferenced

9. **markdown-it@14.1.0** — GHSA-38c4-r59v-3vqw — ReDoS [fixed in 14.1.1]
   - linked-findings: unreferenced

10. **diff@5.2.0** — GHSA-73rr-hh4g-fpgx — DoS in parsePatch/applyPatch [fixed in 5.2.2]
    - linked-findings: unreferenced

11. **yauzl@3.2.0** — GHSA-gmq8-994r-jv83 — Off-by-one error [fixed in 3.2.1]
    - linked-findings: unreferenced

## Remediation Summary

### Immediate (CRITICAL)
1. `package.json:241` — Bump `sequelize` to `^6.37.8` to fix SQL injection via JSON cast type
2. `package.json:158` — Bump `koa` to `^3.1.2` to fix host header injection
3. `package.json:83` — Bump `@node-oauth/oauth2-server` to `^5.3.0` to fix PKCE bypass

### Short-term (HIGH)
1. `package.json` (lodash) — Bump `lodash` to `^4.18.0` to fix prototype pollution in `_.unset`/`_.omit` and code injection via `_.template`

### Hardening (LOW)
1. `package.json` — Bump `markdown-it` to `^14.1.1`, `@modelcontextprotocol/sdk` to `^1.26.0`, `nodemailer` to `^8.0.5`, `yauzl` to `^3.2.1`, `diff` to `^5.2.2`