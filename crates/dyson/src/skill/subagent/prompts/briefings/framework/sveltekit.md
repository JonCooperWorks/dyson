Starting points for SvelteKit — not exhaustive. Three distinct server surfaces: load functions, form actions, and API routes — each with different attack shapes. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**`load` functions** (`+page.server.ts`, `+layout.server.ts`):
- `url.searchParams`, `params` (URL segment), `request` (for POST `load`s), `cookies`, `locals` (set by hooks — trust only if hooks verify).

**Form actions** (`+page.server.ts` default / named `actions`):
- `await request.formData()` → attacker-controlled `FormData`.
- `params`, `url`.

**API routes** (`+server.ts`):
- `await request.json()`, `await request.formData()`, `url`, `params`, `cookies`, `request.headers`.

## Sinks

**SQL (via Drizzle / Prisma / direct DB clients)**
- `db.$queryRawUnsafe(\`... ${formData.get('q')}\`)` — SQLi.  Tagged `$queryRaw\`...\`` is parameterised.
- `pg.query(\`...${user}\`)` — string concat: SQLi.

**Redirect / error helpers**
- `throw redirect(302, url.searchParams.get('next'))` — open redirect.  Always allowlist.
- `throw error(500, userMsg)` where `userMsg` contains sensitive details leaks in dev; OK in production (SvelteKit masks messages), but `dev` builds leak.

**File / path**
- `read(userPath)` via `$lib/server` helpers — traversal.
- `fs.readFileSync(userPath)` in `+server.ts` — traversal.

**Form actions & CSRF**
- SvelteKit's form-actions protect against CSRF via origin checking by default (`csrf: { checkOrigin: true }` in `svelte.config.js`).  Disabling this, OR accepting cross-origin fetches via API routes that do mutations, re-opens CSRF.
- Form actions receive `FormData`; type-narrowing errors (`const q: string = form.get('q') as string`) ignore that `get` returns `File | string | null`.

**Auth in hooks**
- `hooks.server.ts` `handle` callback sets `locals.user = parseJwt(event.cookies.get('token'))` WITHOUT verifying — forged tokens accepted downstream.
- `locals.user = await db.user.findUnique({ where: { id: event.cookies.get('uid') } })` — `uid` cookie trusted as identity without signature check.

**Environment / public vars**
- `$env/static/public` and `PUBLIC_*` env vars are inlined into the CLIENT bundle.  `PUBLIC_API_KEY` = secret leaked to every browser.
- `$env/dynamic/private` is server-only; `$env/dynamic/public` is client-reachable.  Confirm secret names use the right namespace.

**Server-only modules imported from client**
- `$lib/server/*.ts` — SvelteKit errors at build time if imported from a client-only module, BUT `import.meta.glob` or dynamic `await import('$lib/server/x')` from a `+page.ts` bypasses.  Look for dynamic imports of `$lib/server/*`.

**Rate limiting / cookie attrs**
- `cookies.set('session', val, { httpOnly: false })` — stealable via XSS.  Default is httpOnly true if omitted.
- `cookies.set('session', val, { sameSite: 'none', secure: false })` — SameSite=None requires Secure per newer browser policy; some setups ship insecure cookies.

**Output / XSS**
- Svelte components auto-escape `{expr}` output.  `{@html user}` bypasses — raw HTML injection, XSS.
- `+error.svelte` rendering `$page.error.message` with `{@html}` leaks errors as XSS.

**Actions name-vs-logic mismatch**
- Actions exported as `export const actions = { default: async (...) => ..., save: async (...) => ... }` — attacker can pick which action via `?/action` query, including `?/default`.  Some handlers assume only one action shape.

## Tree-sitter seeds (typescript, SvelteKit-focused)

```scheme
; +server.ts handlers — export const GET / POST / ...
(export_statement (lexical_declaration
  (variable_declarator
    name: (identifier) @m
    (#match? @m "^(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD)$"))))

; throw redirect / throw error
(throw_statement (call_expression
  function: (identifier) @f
  (#match? @f "^(redirect|error|fail)$")))
```
