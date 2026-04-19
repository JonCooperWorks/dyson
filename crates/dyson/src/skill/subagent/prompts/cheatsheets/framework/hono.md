Starting points for Hono (Node.js / edge runtimes) — not exhaustive. Edge-first framework running on Cloudflare Workers, Deno, Bun, Node. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`c.req.query("k")`, `c.req.param("id")` (path), `await c.req.json()`, `await c.req.parseBody()`, `c.req.header("H")`, `c.req.cookie("c")`, `await c.req.arrayBuffer()`.

`@hono/zod-validator` or `hono/validator` middleware turns handlers into shape-validated.  Handlers without a `validator('json', schema)` middleware receive raw attacker input.

## Sinks

**SQL (via D1 / Cloudflare / Drizzle / Prisma edge)**
- `c.env.DB.prepare(\`... ${user}\`).run()` — D1's `prepare` accepts string with bind params: `prepare('... ?').bind(user).run()`.  Interpolation is SQLi.
- Drizzle `db.execute(sql.raw(\`...${user}\`))` — raw; use `sql\`...${user}\`` tagged template.
- Prisma `$queryRawUnsafe(\`...${user}\`)` — SQLi.

**File / fetch / SSRF (edge-specific)**
- `await fetch(user_url)` — edge runtimes often have network policies, but server-side fetch to an attacker URL still flows through the runtime's proxy.  SSRF to internal services reachable from the worker's egress.
- Cloudflare Workers: `env.BUCKET.get(user_key)` with R2 — attacker-chosen object key; IDOR if key space isn't scoped to a user.

**Redirect**
- `c.redirect(userUrl)` — open redirect unless validated.
- `c.redirect(userUrl, 301)` — same, harder to undo.

**XSS / HTML**
- `c.html(userHtml)` — raw HTML; no escape.
- `c.html(\`<div>${user}</div>\`)` — template-literal concat with no escape.  Use `hono/html` raw-helper `<Raw>` tags intentionally.

**Command execution (Node/Bun runtimes)**
- `Bun.spawn([user])` — user-controlled binary.
- `child_process.exec(user)` (Node runtime).  Not available in CF Workers runtime.

**Auth middleware**
- `app.use('/admin/*', jwt({ secret: 'dev' }))` — hardcoded secret.
- `app.use('/admin/*', bearerAuth({ token: USER_PROVIDED }))` — static token; insecure.
- Missing middleware on nested routes: `app.route('/admin', adminApp)` where `adminApp` doesn't register the auth middleware.

**Session / cookie**
- `setCookie(c, "s", value, { httpOnly: false, secure: false })` for session-bearing cookies — stealable via XSS / HTTP.
- `getCookie(c, "s")` trusted as identity without signing check.

**JWT**
- `jwt({ secret: ENV.JWT_SECRET })` — good.  `jwt({ secret: 'dev' })` — hardcoded, bad.
- No `algorithms` option → default alg HS256 only, which is fine; explicit `algorithms: ['none']` is a bypass.

**CORS**
- `cors({ origin: "*", credentials: true })` — credentialed wildcard.
- `cors({ origin: (origin) => origin })` — reflects any origin.

## Tree-sitter seeds (javascript / typescript, Hono-focused)

```scheme
; Route: app.get / .post / .all / .route
(call_expression
  function: (member_expression
    object: (identifier) @a
    property: (property_identifier) @m)
  (#eq? @a "app")
  (#match? @m "^(get|post|put|delete|patch|options|all|route|use|basePath)$"))

; c.req.<source> / c.<sink>
(call_expression
  function: (member_expression
    property: (property_identifier) @m)
  (#match? @m "^(query|param|json|parseBody|header|cookie|arrayBuffer|redirect|html|text|notFound|body)$"))
```
