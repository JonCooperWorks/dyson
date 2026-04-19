Starting points for hapi (Node.js) — not exhaustive. Configuration-over-code; routes declared with explicit `options.validate` schemas. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.query`, `request.payload`, `request.params`, `request.headers`, `request.state` (cookies), `request.pre` (pre-handler artifacts — trust only if the pre actually validates).

hapi route definitions take `options.validate: { query, payload, params, headers }` — each Joi / Zod / custom validator.  Absent `validate` = no shape check.

## Sinks

**SQL**
- `db.query(\`... ${request.params.id}\`)` — same SQLi patterns as Express / Fastify.
- Knex `.raw(\`... ${user}\`)` — SQLi.
- Mongoose `.find(request.payload)` — NoSQL operator injection.

**Deserialization / payload**
- `options: { payload: { parse: true, allow: ['application/json', 'application/xml'] } }` — allowing XML opens XXE surface (via underlying parser).
- `options: { payload: { output: 'data', parse: false } }` — handler receives raw `Buffer`; any `pickle`/`binary-parser`-style decode is the finding.

**Auth schemes**
- `server.auth.strategy('session', 'cookie', { cookie: { password: 'dev' } })` — hardcoded signing password.
- `server.auth.default('session')` — sets the default auth strategy globally; routes can opt out with `{ auth: false }`.
- Custom schemes: `server.auth.scheme(name, (server, options) => ({ authenticate: (request, h) => h.authenticated({ credentials: { user: id } }) }))` — authenticating without verifying = bypass.
- `bell` (third-party OAuth): `clientId: "...", clientSecret: "..."` in committed source.

**File / path**
- `h.file(request.params.filename)` — traversal unless `options.confine` is set to the base directory.
- `inert` plugin static-serving: `handler: { directory: { path: userRoot } }` at register time — attacker-derived serve root.
- `response.header('content-disposition', 'attachment; filename="' + request.query.name + '"')` — header injection if `name` has CRLF.

**Redirect**
- `h.redirect(request.query.next)` — open redirect.
- `h.redirect(user_url).permanent()` — same with 301.

**XSS / templates**
- `h.view('template', { user: request.payload.html })` — depends on the view engine; Handlebars `{{{user}}}` unescaped, EJS `<%- user %>` unescaped, Pug `!{user}`.
- `h.response(userHtml).type('text/html')` — raw HTML.

**Validation bypass**
- `options.validate.failAction` set to `'log'` — validation failures logged but request still passes through.  Default is `'error'` (400).  `'ignore'` is explicit bypass.
- `options.validate.options.stripUnknown: true` removes unknown fields silently; combined with mass assignment in handler = attacker-set fields that the validator didn't know about.

**CORS**
- `server.route({ ..., options: { cors: true } })` — permissive default CORS.
- `cors: { origin: ['*'], credentials: true }` — credentialed wildcard.

**State (cookies)**
- `server.state('session', { password: 'dev', isSecure: false, isHttpOnly: false })` — weak cookie config.
- `server.state(..., { encoding: 'iron', password: ..., isSameSite: 'None' })` + `isSecure: false` — SameSite=None requires Secure; browsers reject, but older clients accept.

## Tree-sitter seeds (javascript / typescript, hapi-focused)

```scheme
; server.route({...}) / server.route([...])
(call_expression
  function: (member_expression
    object: (identifier) @s
    property: (property_identifier) @m)
  (#eq? @s "server")
  (#match? @m "^(route|register|state|auth|ext|decorate)$"))

; h.<sink>
(call_expression
  function: (member_expression
    object: (identifier) @h
    property: (property_identifier) @m)
  (#eq? @h "h")
  (#match? @m "^(redirect|response|view|file|authenticated)$"))
```
