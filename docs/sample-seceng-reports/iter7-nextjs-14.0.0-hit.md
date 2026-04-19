# Security Review: Next.js Edge Runtime

## HIGH

### Middleware bypass via forged `x-middleware-subrequest` header
- **File:** `sandbox/sandbox.ts:83`
- **Evidence:**
  ```
  const subreq = params.request.headers[`x-middleware-subrequest`]
  const subrequests = typeof subreq === 'string' ? subreq.split(':') : []
  if (subrequests.includes(params.name)) {
    return {
      waitUntil: Promise.resolve(),
      response: new runtime.context.Response(null, {
        headers: {
          'x-middleware-next': '1',
        },
      }),
    }
  }
  ```
- **Attack Tree:**
  ```
  sandbox/sandbox.ts:83 — attacker sets `x-middleware-subrequest` header to middleware name (e.g. "/middleware")
    └─ sandbox/sandbox.ts:84 — header is split on ':' without provenance check
      └─ sandbox/sandbox.ts:85 — subrequests.includes(params.name) matches → early return
        └─ sandbox/sandbox.ts:88-92 — response returns 'x-middleware-next': '1', skipping middleware execution entirely
  ```
- **Impact:** The `x-middleware-subrequest` header is intended as an internal signal for detecting recursive middleware loops. It is read from `params.request.headers` — the incoming HTTP request — with no verification that it was set by the server. An attacker who sends this header with a value matching the middleware name (`/middleware` or `middleware`) causes the edge runtime to return `x-middleware-next: 1` immediately, bypassing the middleware entirely. Since middleware commonly performs authentication, authorization, rate limiting, and request validation, this allows unrestricted access to protected routes.
- **Exploit:**
  ```
  curl -H "x-middleware-subrequest: /middleware" https://target.com/protected-route
  ```
- **Remediation:** Strip this header from external requests before processing, or verify it was set internally (e.g. by checking for a signed token or requiring it to be sent on an internal-only port/IP). A minimal fix is to not use a plain string header for this control flow decision:
  ```typescript
  // Verify provenance — do not trust header from external requests
  const subreq = params.request.headers[`x-nextjs-internal-subrequest`] // rename to internal-prefixed
  // Or: strip the header upstream in the server/proxy layer before it reaches edge runtime
  ```

## MEDIUM

### Hardcoded preview mode credentials used as production fallback
- **File:** `edge-route-module-wrapper.ts:103`
- **Evidence:**
  ```
  preview: prerenderManifest?.preview || {
    previewModeEncryptionKey: '',
    previewModeId: 'development-id',
    previewModeSigningKey: '',
  },
  ```
  Also present at `adapter.ts:177-180` and `adapter.ts:203-205`.
- **Attack Tree:**
  ```
  edge-route-module-wrapper.ts:90-93 — prerenderManifest parsed from self.__PRERENDER_MANIFEST
  edge-route-module-wrapper.ts:103 — if manifest preview is missing, falls back to hardcoded values
    └─ empty signing/encryption keys + known previewModeId 'development-id' allow forging preview-mode cookies
  ```
- **Impact:** If the prerender manifest is not properly serialized into the edge runtime context (which can occur in certain deployment configurations or custom builds), the fallback preview credentials have empty signing/encryption keys and a well-known `previewModeId` value. This allows an attacker to forge valid preview-mode cookies, bypassing static revalidation and potentially accessing draft content. The severity is MEDIUM because production deployments normally include the manifest, making this a fallback-only path.
- **Remediation:** Throw an error when prerenderManifest is missing in production, rather than falling back to hardcoded development credentials:
  ```typescript
  if (!prerenderManifest?.preview) {
    throw new Error('Invariant: prerenderManifest.preview is required in edge runtime');
  }
  ```

## LOW / INFORMATIONAL

No findings.

## Checked and Cleared

- `sandbox/context.ts:481` — `runInContext(content, moduleContext.runtime.context)` executes file contents from `evaluateInContext(filepath)`. The `filepath` values come from `params.paths` in `sandbox/sandbox.ts:75-76`, which are build-time file paths supplied by Next.js, not user input. Safe.
- `sandbox/context.ts:252-267` — `__next_eval__` wraps function execution with a warning. Only a warning is emitted, execution still proceeds (line 267 `return fn()`). However, `__next_eval__` is never called in-scope; it's defined on `context` for compiled edge-runtime code. The compiled edge runtime code that uses it is trusted build output. Safe within scope.
- `adapter.ts:72` — `JSON.parse(self.__PRERENDER_MANIFEST)` parses `self.__PRERENDER_MANIFEST`, which is a build-injected string, not user-controlled. Safe.
- `spec-extension/unstable-cache.ts:161` — `JSON.parse(resData.body)` parses from incremental cache entries (server-stored data), not user input. Safe.
- `sandbox/context.ts:99-103` — `buildEnvironmentVariablesFrom()` copies all of `process.env` into the edge runtime context. This exposes server environment variables to edge runtime code, but the edge runtime runs trusted server-side code, not user code. The risk is only if user code runs in the edge runtime — which it does (middleware), but the edge runtime is sandboxed and only server-side. This is a design choice, not a vulnerability in the reviewed scope.
- `sandbox/context.ts:17` — `import { runInContext } from 'vm'` — imported but only used at line 481 with build-time paths per above. Safe.
- `sandbox/fetch-inline-assets.ts:28` — `resolve(options.distDir, asset.filePath)` resolves asset paths from build-time manifest (`EdgeFunctionDefinition['assets']`), not user input. Safe.
- `globals.ts:32-50` — `__import_unsupported` creates proxy objects for unsupported Node.js modules. The proxy throws on access, does not execute user code. Safe.
- `adapter.ts:301` — `response.headers.delete('Location')` and `adapter.ts:302-305` — redirects use `x-nextjs-redirect` header for data requests. This is framework design, not injection. Safe.
- `spec-extension/response.ts:87` — `headers.set('Location', validateURL(url))` — URL is validated before use. Safe.

## Dependencies

27 vulnerabilities across 15 dependencies detected by dependency_review. Critical/High findings with linked source files:

**Critical:**
- **path-to-regexp@6.1.0** — GHSA-9wv6-86v6-598j — ReDoS backtracking in route matching regex. [fixed ≥ 6.3.0]
  linked-findings: `packages/next/src/shared/lib/router/utils/path-match.ts:2`, `packages/next/src/shared/lib/router/utils/prepare-destination.ts:8`, `packages/next/src/lib/load-custom-routes.ts:2`
- **webpack@5.86.0** — GHSA-4vvj-4cpr-p986 — DOM Clobbering Gadget in AutoPublicPathRuntimeModule leads to XSS in client bundles. [fixed ≥ 5.94.0]
- **@babel/traverse@7.22.5** — GHSA-67hx-6x53-jw92 — Arbitrary code execution when compiling crafted malicious code. [fixed ≥ 7.23.2]

**High:**
- **devalue@2.0.1** — GHSA-cfw5-2vxh-hr84 — Prototype pollution via `devalue.parse` / `devalue.unflatten`. [fixed ≥ 5.6.4]
  linked-findings: build manifest serialization (build-time surface)
- **tar@6.1.15** — GHSA-83g3-92jg-28cx — Arbitrary File Read/Write via Hardlink Target Escape through Symlink Chain. [fixed ≥ 7.5.8]
  linked-findings: `packages/next/src/lib/download-swc.ts:4` (SWC binary download)
- **cookie@0.4.1** — GHSA-pxg6-pf52-xh8x — Cookie parser accepts out-of-bounds characters in name, path, domain. [fixed ≥ 0.7.0]
  linked-findings: `packages/next/src/server/api-utils/index.ts:3`, `packages/next/src/server/api-utils/node/api-resolver.ts:5`

- No lockfile detected — versions resolved from ranges; actual installed versions may differ.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `sandbox/sandbox.ts:83` — Strip or sign `x-middleware-subrequest` header; do not trust external request headers for internal control flow.
2. Bump path-to-regexp ≥ 6.3.0 to fix ReDoS in route matching.
3. Bump webpack ≥ 5.94.0 (ideally ≥ 5.105.0) to fix DOM Clobbering XSS.
4. Bump @babel/traverse ≥ 7.23.2 to fix code execution during compilation.
5. Bump devalue ≥ 5.6.4 to fix prototype pollution.
6. Bump tar ≥ 7.5.8 to fix arbitrary file read/write via symlink escape.
7. Bump cookie ≥ 0.7.0 to fix out-of-bounds character acceptance.

### Short-term (MEDIUM)
1. `edge-route-module-wrapper.ts:103` — Fail hard when prerender manifest is missing in production instead of falling back to hardcoded development credentials.
2. Generate and commit a `package-lock.json` or `yarn.lock` to prevent version drift.

### Hardening (LOW)
1. `sandbox/context.ts:99-103` — Audit whether all `process.env` values should be exposed to the edge runtime context; consider an allowlist.