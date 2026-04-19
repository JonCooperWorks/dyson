Starting points for Nuxt (Vue full-stack) — not exhaustive. Server routes + SSR + Nitro engine underneath.  Similar surfaces to Next.js / SvelteKit. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**Server API routes** (`server/api/**/*.ts`, `server/routes/**/*.ts`):
- `event` object is the attacker-observable — use `getQuery(event)`, `readBody(event)`, `getRouterParam(event, 'id')`, `getHeader(event, 'H')`, `parseCookies(event)`.

**Server middleware** (`server/middleware/*.ts`):
- Runs on every request.  `event` same as above.

**`useFetch` / `useAsyncData` in Nuxt pages** — client-side; the server does NOT see attacker URL here (the client chose it).  Concern: ensure `$fetch(url)` on the server side isn't user-controlled (SSRF).

## Sinks

**SQL (via Drizzle / Prisma / unstorage drivers)**
- `db.$queryRawUnsafe(\`... ${readBody(event).x}\`)` — SQLi; use `$queryRaw` tagged template.
- Drizzle `db.execute(sql.raw(\`...${user}\`))` — raw; use `sql\`...\``.

**`$fetch` / SSRF**
- `$fetch(userUrl)` in a server route — SSRF, no default host allowlist.
- `useFetch(url)` server-side (during SSR) — if `url` is derived from server-side input, same SSRF class.

**Redirect**
- `sendRedirect(event, userUrl, 302)` — open redirect unless validated.
- `throw createError({ ..., data: { redirect: userUrl } })` with a client-side handler — same.

**File / path**
- `sendStream(event, createReadStream(userPath))` — traversal.
- `Nuxt` static serving `public/` folder is fine; `server/routes/files/[name].ts` reading `readFile(\`public/\${event.context.params.name}\`)` is traversal unless basename + anchor.

**Nitro config / runtime**
- `nitro.storage` with user-derived keys — for Redis/KV backends, attacker picks cache keys.
- `nitro.experimental.wasm` — enabling WebAssembly with dynamic module loads from user input = RCE.

**`useRuntimeConfig()`**
- `runtimeConfig.public.*` is EXPOSED to the client.  `runtimeConfig.public.apiKey = process.env.API_KEY` = secret in every browser.
- `runtimeConfig.x` (without `.public`) is server-only; safe.
- Confirm `apiSecret` / `jwtSecret` / similar secret names are NOT under `public`.

**Auth middleware**
- `useSession(event)` (via `h3-session` or `nuxt-auth-utils`) — session storage pluggable; check store backend + signing key.
- Custom `event.context.user = jwt.verify(token, 'dev')` — hardcoded secret.
- Missing auth middleware on a `server/api/admin/*.ts` route — routes under `server/api/` are public by default.

**XSS**
- Vue templates auto-escape `{{ user }}`.  `v-html="user"` bypasses.
- `useHead({ script: [{ innerHTML: user }] })` — inject script tag with user content.

**Nuxt modules / auto-imports**
- Auto-imports pull `utils/*.ts` into every page; a server-only helper accidentally imported into a client page can leak server secrets into the bundle.  Use `server/utils/` (server-only) vs `utils/` (universal).

**OAuth / social auth**
- `nuxt-auth-utils` / `@sidebase/nuxt-auth` — OAuth callback handlers MUST verify state parameter; custom implementations may skip.

## Tree-sitter seeds (typescript, Nuxt-focused)

```scheme
; defineEventHandler / createError / sendRedirect / readBody / getQuery
(call_expression
  function: (identifier) @f
  (#match? @f "^(defineEventHandler|defineNuxtRouteMiddleware|defineNitroPlugin|createError|sendRedirect|sendStream|send|readBody|readValidatedBody|getQuery|getValidatedQuery|getRouterParam|getRouterParams|getCookie|setCookie|getHeader|setHeader|parseCookies)$"))

; $fetch / useFetch — SSRF surface when URL comes from body/query
(call_expression
  function: (identifier) @f
  (#match? @f "^(\\$fetch|useFetch|useAsyncData|useLazyFetch)$"))
```
