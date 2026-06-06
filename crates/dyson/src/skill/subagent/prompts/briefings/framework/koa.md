Starting points for Koa (Node.js) — not exhaustive. Lightweight successor to Express by the same team; middleware model is the whole framework. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`ctx.query`, `ctx.request.body` (typically populated by `koa-bodyparser`), `ctx.params` (via `@koa/router`), `ctx.headers`, `ctx.cookies.get("c")`, `ctx.request.files` (via `@koa/multer`).

Koa has NO body parser built in — without `koa-bodyparser` / `koa-body`, `ctx.request.body` is undefined.  If body-parsing middleware is only installed on some routes, others receive no body validation by default.

## Sinks

**SQL / NoSQL**
- `db.query(\`SELECT ... ${ctx.params.id}\`)` — template-literal concat: SQLi.  Use parameter placeholders.
- Mongoose `.find(ctx.request.body)` — operator injection (`{ $gt: "" }`).

**Command execution**
- `child_process.exec(ctx.query.cmd)` — RCE.
- `spawn('bash', ['-c', user])` — shell RCE.

**File / path**
- `ctx.attachment = ctx.query.path` — traversal.
- `koa-static(userRoot)` at mount time — attacker-derived `userRoot`.
- `ctx.body = fs.createReadStream(ctx.query.file)` — direct disclosure.

**Redirect**
- `ctx.redirect(ctx.query.next)` — open redirect.
- `ctx.redirect(ctx.query.next, '/fallback')` — same if `next` is attacker-controlled.

**XSS / templates**
- `ctx.type = 'text/html'; ctx.body = userHtml` — raw HTML body.
- `@koa/views` rendering `nunjucks` / `handlebars` / `ejs` — unescaped output helpers bypass auto-escape.

**Body parser config**
- `koa-bodyparser({ enableTypes: ['json', 'form', 'text', 'xml'] })` with `xml: true` — XML body parsing adds XXE surface (via `libxmljs` etc.).
- `koa-body({ multipart: true, formidable: { uploadDir: userDir } })` — attacker-derived upload dir = traversal at write time.

**Authentication / sessions**
- `koa-session` with no `key` / `keys` array — signing disabled; sessions forgeable.
- `ctx.session` mutations without `ctx.session.save()` in some setups — silent session loss.
- Custom middleware `app.use(async (ctx, next) => { ctx.state.user = parseJwt(ctx.headers.authorization); await next(); })` — parses JWT without verifying signature, then downstream relies on `ctx.state.user`.  Forged tokens accepted.

**CORS (`@koa/cors`)**
- `cors({ origin: true, credentials: true })` — reflects any origin with credentials.
- `origin: '*'` + `credentials: true` — browsers reject, but attackers probe with `origin: /.*/` or the reflective form.

**Error handling**
- Default Koa error emitter logs `err.stack` which may include secrets from `error` objects constructed with context.
- `app.on('error', (err, ctx) => console.error(err))` without stripping sensitive request data → log poisoning.

## Tree-sitter seeds (javascript / typescript, Koa-focused)

```scheme
; router.get / .post / .use
(call_expression
  function: (member_expression
    object: (identifier) @r
    property: (property_identifier) @m)
  (#match? @r "^(router|app)$")
  (#match? @m "^(get|post|put|delete|patch|options|head|all|use)$"))

; ctx.<source / sink>
(call_expression
  function: (member_expression
    object: (identifier) @c
    property: (property_identifier) @m)
  (#eq? @c "ctx")
  (#match? @m "^(redirect|attachment|throw|assert)$"))

(member_expression
  object: (identifier) @c
  property: (property_identifier) @p
  (#eq? @c "ctx")
  (#match? @p "^(query|body|params|headers|cookies|request)$")) @src
```
