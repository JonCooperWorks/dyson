# Security Review: Next.js 14.0.0 — packages/next/src/server/web

## CRITICAL

### Middleware authorization bypass via `x-middleware-subrequest` header (CVE-2025-29927)

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
  External attacker sets header: GET /any-page
    └─ HTTP header: x-middleware-subrequest: middleware
       └─ sandbox/sandbox.ts:83 — params.request.headers['x-middleware-subrequest'] reads attacker value
          └─ sandbox/sandbox.ts:84 — splits "middleware" by ':' → ["middleware"]
             └─ sandbox/sandbox.ts:85 — "middleware".includes("middleware") → true
                └─ sandbox/sandbox.ts:86-93 — early return with x-middleware-next:1, skips middleware function entirely (lines 96-125 never execute)
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=typescript, files=47, defs=312, calls=1847, unresolved_callees=41
  
  Found 1 candidate path(s) from sandbox/sandbox.ts:83 to sandbox/sandbox.ts:85:
  
  Path 1 (depth 2, resolved 2/2 hops):
    packages/next/src/server/web/sandbox/sandbox.ts:83 [byte 2580-2650] — fn `runWithTaggedErrors` — taint root: params.request.headers
    └─ packages/next/src/server/web/sandbox/sandbox.ts:85 [byte 2710-2745] — [SINK REACHED] — tainted at sink: subrequests.includes(params.name) early-return bypass
  ```
- **Impact:** Any middleware registered by the application — including authentication checks, authorization / role guards, rate limiting, CORS policies, request validation, and audit logging — is completely skipped when the attacker supplies `x-middleware-subrequest: <middleware-name>` as an HTTP request header. The response at line 88-92 carries `x-middleware-next: 1`, signaling to the Next.js upstream router that middleware ran and the request should proceed directly to the route handler. All protected endpoints become accessible to unauthenticated users.
- **Exploit:**
  ```
  curl -H "x-middleware-subrequest: middleware" https://target.com/protected-endpoint
  ```
  For a middleware named `/src/middleware` the header value would be `src/middleware`. The middleware name is the file path relative to the project root, discoverable from common conventions (`middleware`, `src/middleware`) or via error responses.
- **Remediation:** The `x-middleware-subrequest` header must be treated as an internal-only signal and stripped from all externally-originating requests before reaching the sandbox. Either:
  1. Strip it at the Node.js HTTP server layer before constructing `RequestData`, or
  2. Add it to the `FORBIDDEN_HEADERS` blocklist at `sandbox/sandbox.ts:10-14`:
  ```ts
  const FORBIDDEN_HEADERS = [
    'content-length',
    'content-encoding',
    'transfer-encoding',
    'x-middleware-subrequest',  // prevent client-controlled middleware bypass
  ]
  ```

## HIGH

### No additional high-severity findings within the scoped `server/web` subpath

No findings.

## LOW / INFORMATIONAL

### `x-middleware-subrequest` is propagated downstream in fetch polyfill

- **File:** `sandbox/context.ts:329`
- **Evidence:**
  ```
  const prevs = init.headers.get(`x-middleware-subrequest`)?.split(':') || []
  const value = prevs.concat(options.moduleName).join(':')
  init.headers.set('x-middleware-subrequest', value)
  ```
- **Impact:** The same header is appended to on every internal `fetch()` call, building a chain of middleware names. For loop-prevention (internal subrequest detection), this is correct behavior. However, because the header origin is not validated at the entry point, a client-supplied value seeds this chain. Informational because the primary bypass is already captured at `sandbox/sandbox.ts:83`.

## Checked and Cleared

- `sandbox/sandbox.ts:10-14` — `FORBIDDEN_HEADERS` strips `content-length`, `content-encoding`, `transfer-encoding` from responses; does NOT strip `x-middleware-subrequest` (this is the finding).
- `sandbox/context.ts:241-250` — custom `require` polyfill resolves only from `NativeModuleMap` (line 244); rejects unknown IDs with `TypeError`. No arbitrary require exploitation.
- `sandbox/context.ts:252-268` — `__next_eval__` wraps eval with a one-time warning per unique function string. Eval is not blocked in production (`process.env.NODE_ENV !== 'production'` enables `codeGeneration.strings: true` at line 235), but calling code uses `__next_eval__` only for developer warning; no direct eval of user input was found in this scope.
- `adapter.ts:112` — `fromNodeOutgoingHttpHeaders` converts headers; no sanitization of `x-middleware-subrequest` — it passes through to the sandbox.
- `utils.ts:137-148` — `validateURL` validates URL parseability only; no security impact.
- `exports/*.ts` — public API surface exports; no direct HTTP handling.
- `spec-extension/*.ts` — type/adaptation layer for Edge API compatibility; no HTTP sink found.

## Dependencies

No vulnerable dependencies found within the scoped `packages/next/src/server/web` subpath (this directory contains only TypeScript source files, no dependency manifests). The dependency_review subagent reported findings for a different project (juice-shop) outside the review scope — disregarded.

## Remediation Summary

### Immediate (CRITICAL)
1. `sandbox/sandbox.ts:83` — Add `x-middleware-subrequest` to `FORBIDDEN_HEADERS` (or strip from incoming `params.request.headers` at the Node.js server layer) to prevent client-controlled bypass of the middleware pipeline.

### Short-term (MEDIUM)
No findings.

### Hardening (LOW)
1. `sandbox/context.ts:329` — Validate that the initial `x-middleware-subrequest` value is empty before appending `moduleName`, in case a future code path changes how headers enter the sandbox.