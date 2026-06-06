Starting points for Fastify (Node.js) — not exhaustive. Schema-first design closes classes Express leaves open, but the escape hatches are sharp. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.query`, `request.body`, `request.params`, `request.headers`, `request.cookies` (via `@fastify/cookie`), `request.raw`.

Fastify validates request bodies against a JSON Schema declared on the route — this is one of the biggest security wins.  A route with `schema: { body: ... }` auto-rejects unknown fields when `additionalProperties: false` is set.  Handlers that DON'T declare a schema receive raw unvalidated bodies, same risk as Express.

## Sinks

**SQL injection**
- `@fastify/postgres`: `await fastify.pg.query(\`SELECT ... ${user}\`)` — template-literal concat is SQLi.  Use placeholder syntax: `fastify.pg.query('... $1', [user])`.
- `@fastify/mysql`: `await fastify.mysql.query(\`...${user}\`)` — same.
- Prisma: `prisma.$queryRawUnsafe(\`...${user}\`)` — SQLi.  Tagged template `prisma.$queryRaw\`...\`` is parameterised.

**Deserialization / schema bypass**
- Routes without a `schema` definition accept arbitrary JSON.  Combined with downstream `body[user_key]` walks or `Object.assign(target, body)`, that's the prototype-walk / pollution primitive.
- `additionalProperties: true` (default in some setups) + mass-assignment into a DB record = unintended columns set.
- Custom JSON body parsers (`fastify.addContentTypeParser`) that call `JSON.parse` on a raw stream then hand off without shape validation.

**Command execution**
- `child_process.exec(request.query.cmd)` in a handler — RCE.
- `spawn('bash', ['-c', user])` — shell RCE.

**File / path**
- `@fastify/static` with `root: path.join(process.cwd(), req.query.dir)` at plugin registration — traversal if `dir` is user-derived.
- `reply.sendFile(request.query.name)` — traversal unless `name` is basename-stripped and prefix-checked.

**Redirect / SSRF**
- `reply.redirect(request.query.next)` — open redirect.
- `@fastify/reply-from` proxying to `request.query.target` — SSRF.
- `fetch(request.body.url)` / `axios.get(request.body.url)` — SSRF.

**XSS**
- `reply.type('text/html').send(user_html)` — raw HTML body.
- `@fastify/view` with `engine: { handlebars }` — `{{{user}}}` unescaped; same for EJS `<%- user %>`.

**Auth / JWT**
- `@fastify/jwt` with `secret: 'dev'` — hardcoded secret.
- `algorithm: 'none'` explicit bypass.
- `fastify.authenticate` as `request.jwtVerify()` without wrapping in a `preHandler` for protected routes.

**Hooks ordering**
- `preHandler`, `preValidation`, `onRequest` — a `preValidation` hook that reads request data BEFORE schema validation sees raw attacker input; if it does anything non-trivial (DB lookup, sensitive operation), that's unvalidated-data-at-handler.
- Missing global `onRequest: auth` hook — per-route auth only.

**CORS**
- `@fastify/cors` with `origin: true, credentials: true` — reflects origin with credentials.
- `origin: '*'` + `credentials: true` — browsers reject, but `origin: /.*/` regex matches everything and is equally bad.

**Rate limits (out of scope per rules; mentioned for completeness)**
- `bodyLimit: Infinity` / `1GB` on a public endpoint — memory DoS.  Not a finding unless it yields memory corruption.

## Tree-sitter seeds (javascript / typescript, Fastify-focused)

```scheme
; Route registration: fastify.get / .post / etc. with schema option
(call_expression
  function: (member_expression
    object: (identifier) @f
    property: (property_identifier) @m)
  (#eq? @f "fastify")
  (#match? @m "^(get|post|put|delete|patch|options|head|route|register)$"))

; reply.redirect / reply.send / reply.sendFile
(call_expression
  function: (member_expression
    object: (identifier) @r
    property: (property_identifier) @m)
  (#eq? @r "reply")
  (#match? @m "^(redirect|send|sendFile|type|header)$"))
```
