# Security Review: BookStack v23.10 (app/ source tree)

## CRITICAL

### Hardcoded default encryption key in source
- **File:** `Config/app.php:104`
- **Evidence:**
  ```php
  'key' => env('APP_KEY', 'AbAZchsay4uBTU33RubBzLKw203yqSqr'),
  ```
- **Attack Tree:**
  ```
  Config/app.php:104 — default fallback key committed in source
    └─ Http/Middleware/EncryptCookies.php — session cookie encrypted with this key
      └─ attacker decrypts/forges session cookie → arbitrary user impersonation
  ```
- **Impact:** When `APP_KEY` is not set in the environment (a common misconfiguration), Laravel uses this committed default key. An attacker who reads this value can: (a) decrypt any active session cookie and impersonate any logged-in user including administrators; (b) forge new session cookies to create arbitrary authenticated sessions; (c) decrypt MFA backup codes stored in the `mfa_values` table (`Access/Mfa/MfaValue.php:74` encrypts with the same key); (d) decrypt cached SAML responses (`Access/Controllers/Saml2Controller.php:91` encrypts with the same key). This is a committed default credential that breaks all cryptographic guarantees of the application.
- **Exploit:**
  ```bash
  curl -H "Cookie: XSRF-TOKEN=; laravel_session=<attacker-forged-session>" http://bookstack/settings
  ```
  (Where the forged session cookie is produced by `openssl_encrypt` with AES-256-CBC and the key `AbAZchsay4uBTU33RubBzLKw203yqSqr`.)
- **Remediation:** Remove the fallback default key entirely. Crash at startup if `APP_KEY` is not set:
  ```php
  'key' => env('APP_KEY') ?: throw new \RuntimeException('APP_KEY must be set in environment.'),
  ```

## MEDIUM

### SAML2 POST routes CSRF-exempt via broad wildcard pattern
- **File:** `Http/Middleware/VerifyCsrfToken.php:21-23`
- **Evidence:**
  ```php
  protected $except = [
      'saml2/*',
  ];
  ```
- **Attack Tree:**
  ```
  Http/Middleware/VerifyCsrfToken.php:22 — wildcard 'saml2/*' excludes ALL SAML routes from CSRF verification
    └─ POST /saml2/login (line 321 in web.php) — CSRF-exempt SAML login initiation
    └─ POST /saml2/logout (line 322 in web.php) — CSRF-exempt SAML logout
  ```
- **Impact:** An attacker can craft a malicious page that auto-submits a forged POST to `/saml2/login` or `/saml2/logout`, causing any visiting BookStack user to be redirected to the SAML IdP login flow or SAML logout flow. This is limited to initiating IdP-external flows (the victim is redirected to the IdP, not logged in or out directly). The `/saml2/acs` POST endpoint must be CSRF-exempt because it receives external POSTs from the IdP — that exclusion is correct. The `/saml2/login` and `/saml2/logout` POSTs do not need broad CSRF exemption.
- **Remediation:** Narrow the exception to only the ACS and SLS endpoints:
  ```php
  protected $except = [
      'saml2/acs',
      'saml2/sls',
  ];
  ```

## LOW / INFORMATIONAL

### No findings.

The following areas were examined and found to have appropriate mitigations:

- `Search/SearchRunner.php:357` — `whereRaw` tag value is PDO-quoted, quotes stripped, then `(float)` cast. Numeric-only, no injection.
- `Search/SearchRunner.php:214` — `DB::raw($scoreSelect['statement'])` uses parameterized bindings via `selectForScoredTerms`.
- `Search/SearchRunner.php:487` — `DB::raw` comment subquery uses `DB::getTablePrefix()` and `$model->getMorphClass()` (trusted model identifiers).
- `Entities/Tools/PageContent.php:376` — `parsePageIncludes` uses `intval()` on the page ID portion of include tags.
- `Util/HtmlContentFilter.php:16-69` — strips `<script>` tags, `on*` attributes, `javascript:` URIs, SVG data/javascript URIs, meta refresh, and xlink:href.
- `Access/Controllers/OidcController.php:48-49` — state parameter validated against session-stored value.
- `Api/ApiTokenGuard.php:140` — token secret compared via `Hash::check()` (timing-safe bcrypt).
- `Api/ListingResponseBuilder.php:116-124` — filter field names validated against a whitelist; operators mapped to known-safe values.
- `Config/saml2.php:41` — `'strict' => true` enforces SAML signature validation.
- `Util/SsrUrlValidator.php:54` — SSR host pattern is anchored with `^` and suffix-constrained, preventing subdomain injection.
- `Http/Middleware/TrustProxies.php:36` — wildcard `**`/`*` proxy trust is opt-in via config, defaults to no trusted proxies.

## Checked and Cleared

- `Search/SearchRunner.php:357` — `whereRaw` uses `(float)` cast after PDO quoting; value is numeric only.
- `Search/SearchRunner.php:214` — `DB::raw` score statement uses parameterized bindings, not string interpolation.
- `Search/SearchRunner.php:286-288` — `selectRaw`/`groupByRaw` use parameterized `$whenBindings`.
- `Search/SearchRunner.php:487` — `DB::raw` comment subquery uses trusted `getTablePrefix()` and `getMorphClass()`.
- `Search/SearchRunner.php:474-476` — `filterSortBy` method dispatch validates `method_exists($this, ...)`; only existing methods callable.
- `Permissions/PermissionApplicator.php:105-109` — `selectRaw`/`havingRaw` use parameterized bindings with `?`.
- `Permissions/PermissionApplicator.php:168` — `selectRaw` uses single-quoted literal `{$entity->getMorphClass()}` (trusted model).
- `Entities/Controllers/PageRevisionController.php:41` — `selectRaw` uses constant string `"IF(markdown = '', false, true) as is_markdown"`.
- `Activity/TagRepo.php:34-39` — `DB::raw` calls use constant aggregate expressions, not user data.
- `Activity/TagRepo.php:69,91` — `DB::raw('count(*)')` — constant expression.
- `Entities/Queries/Popular.php:18` — `DB::raw('SUM(views)')` — constant expression.
- `Entities/Models/Entity.php:94` — `selectRaw('SUM(views)')` — constant expression.
- `Users/Models/User.php:305` — `selectRaw('max(created_at)')` — constant expression.
- `Console/Commands/UpgradeDatabaseEncodingCommand.php:37` — `DB::select('SHOW TABLES')` — no user input.
- `Entities/Tools/PageContent.php:376` — `{{@<page_id>#section}}` include tags parse page ID via `intval()`.
- `Entities/Tools/PageContent.php:301` — scripts removed from HTML via `HtmlContentFilter::removeScripts()`.
- `Access/Controllers/Saml2Controller.php:91` — `encrypt($samlResponse)` with random 16-char cache key.
- `Access/Controllers/Saml2Controller.php:119` — `processAcsResponse` validates SAML signature via onelogin toolkit.
- `Access/Controllers/OidcController.php:57` — state parameter validated; access token exchange via league/oauth2.
- `Api/ApiTokenGuard.php:98-109` — API token validated via DB lookup + `Hash::check()` (bcrypt, timing-safe).
- `Api/ApiTokenGuard.php:144-146` — token expiration checked with `Carbon::now()`.
- `Http/Middleware/ApiAuthenticate.php:34-56` — session-based API access checks `access-api` permission and `hasAppAccess()`.
- `Http/Middleware/Authenticate.php:15` — `hasAppAccess()` blocks guest users from authenticated routes.
- `Http/Middleware/CheckUserHasPermission.php:21` — permission enforcement via `user()->can($permission)`.
- `Util/SsrUrlValidator.php:54` — SSR host regex is anchored `^...$` with suffix constraint `($|\/.*$|#.*$)`.
- `Util/HtmlContentFilter.php:29-61` — removes `<script>`, `javascript:` URIs, `on*` attrs, SVG data/js URIs, form `action="javascript:"`, meta refresh.
- `Uploads/AttachmentService.php:219` — filename randomized with `Str::random(16)`.
- `Uploads/AttachmentService.php:57` — path normalizer strips traversal via `WhitespacePathNormalizer`.
- `Uploads/ImageStorage.php:82-110` — `urlToPath` validates image URL starts with `uploads/images/` or matches known host paths.
- `Uploads/ImageService.php:239-243` — `pathAccessibleInLocalSecure` checks file exists, is image MIME, and optionally enforces page/book visibility.

## Dependencies

### Critical/High (linked findings)
- **laravel/framework@9.52.16** — GHSA-78fx-h6xr-vch4 (File Validation Bypass) and GHSA-gv7v-rgg6-548h (Query string env manipulation). The file validation bypass is relevant to BookStack's image/attachment uploads which use Laravel's `mimes`/`image` validation rules.
- **league/commonmark@2.4.1** — GHSA-c2pc-g5qf-rfrf (quadratic complexity DoS), GHSA-hh8v-hgvp-g3f5 (embed extension allowed_domains bypass), GHSA-3527-qv2q-pfvx (XSS in Attributes extension), GHSA-4v6x-c7xx-hw9f (DisallowedRawHtml bypass via whitespace). BookStack uses commonmark for Markdown-to-HTML conversion (`Entities/Tools/Markdown/MarkdownToHtml.php`). The XSS bypass is relevant if any user-supplied Markdown is rendered.
- **dompdf/dompdf@2.0.3** — GHSA-3qx2-6f78-w2j2 (SVG parse DoS). Used for PDF export (`Entities/Tools/PdfGenerator.php:22`). User-controlled page HTML is fed to dompdf.
- **nesbot/carbon@2.71.0** — GHSA-j3f9-p6hm-5w6q (file include via `setLocale`). Not directly exposed in BookStack — locale is set via config/env.
- **phpseclib/phpseclib@3.0.23** — GHSA-r854-jrxh-36qx (variable-time HMAC). Multiple other advisories (DoS, name confusion, OID length, AES-CBC padding oracle).
- **robrichards/xmlseclibs@3.1.1** — GHSA-c4cc-x928-vjw9 (Libxml2 canonicalization bypass), GHSA-4v26-v6cg-g6f9 (AES-GCM tag validation). Used by onelogin/php-saml for SAML signature verification.
- **onelogin/php-saml@4.1.0** — GHSA-5j8p-438x-rgg5 (xmlseclibs CVE-2025-66475). Direct SAML validation impact.
- **aws/aws-sdk-php@3.283.8** — CloudFront policy injection, URI path traversal, S3 encryption key commitment issue (if S3 storage is configured).
- **phenx/php-svg-lib@0.5.0** — restriction bypass/potential RCE, path validation, SVG recursion DoS (dompdf dependency).
- **symfony/http-foundation@6.0.20** — PATH_INFO auth bypass, open redirect via browser-sanitized URLs.

No `composer.json` or `composer.lock` files were found in the reviewed `app/` directory (they exist in the parent project root). Scan was performed against `composer.lock` from `../composer.lock`.

### Summary
Multiple vulnerable dependencies at CRITICAL/HIGH severity. The laravel/framework, league/commonmark, dompdf, onelogin/php-saml, and xmlseclibs vulnerabilities are most impactful given BookStack's use patterns. All should be upgraded.

## Remediation Summary

### Immediate (CRITICAL)
1. `Config/app.php:104` — Remove hardcoded default `APP_KEY`; crash at startup if not set via environment.

### Short-term (MEDIUM)
1. `Http/Middleware/VerifyCsrfToken.php:21-23` — Narrow CSRF exception from `saml2/*` to `saml2/acs` and `saml2/sls` only.

### Hardening (LOW)
1. Dependency upgrades: `laravel/framework ≥ 9.52.17`, `league/commonmark ≥ 2.8.2`, `dompdf ≥ 2.0.4`, `onelogin/php-saml ≥ 4.3.1`, `robrichards/xmlseclibs ≥ 3.1.5`, `phpseclib ≥ 3.0.51`