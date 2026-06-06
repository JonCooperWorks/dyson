Starting points for AdonisJS (Node.js) — not exhaustive. Laravel-inspired TypeScript-first framework with full MVC + ORM. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.input('k')`, `request.qs()`, `request.body()`, `request.param('id')`, `request.header('H')`, `request.cookie('c')`, `request.file('f')`, `request.only([...])` / `request.except([...])` (field selection).

`request.validateUsing(validator)` runs a VineJS schema — the proper validation path.  `request.input('k')` without validation returns untyped.

## Sinks

**SQL (Lucid ORM)**
- `Database.rawQuery(\`SELECT ... '${user}'\`)` — SQLi; use `Database.rawQuery('... ?', [user])`.
- `.whereRaw(\`name = '${user}'\`)` — SQLi; `.whereRaw('name = ?', [user])` is safe.
- Lucid models: `await User.query().where('name', user)` — safe (parameterised).  `.where(\`name = '${user}'\`)` — SQLi.

**Mass assignment**
- `User.create(request.body())` — without `$fillable` / `$guarded` on the model, attacker sets any column.  Lucid supports `serialize`/`$fillable`-style protection.
- `user.merge(request.body()).save()` — same.
- `request.only(['name', 'email'])` — explicit allowlist; safer.

**Deserialization**
- `@poppinss/utils` `unserialize` (rare) — PHP-like unserialize patterns don't apply; JSON body parsing via `request.body()` is safe-shape.
- Custom middleware decoding raw `request.request` (underlying Node.js IncomingMessage) with `node-serialize.unserialize` — RCE.

**Command execution**
- `execa(user)`, `child_process.exec(user)` — RCE.

**File / path**
- `response.download(userPath)`, `response.attachment(userPath)` — traversal.
- `request.file('upload')!.clientName` — attacker filename.  `file.move(Application.tmpPath(), { name: userName })` — traversal without basename + allowlist.
- `Drive.get(userPath)` — if filesystem driver, traversal; if S3 driver, attacker-chosen key (IDOR).

**Redirect**
- `response.redirect(userUrl)` — open redirect.
- `response.redirect().toPath(userPath)` — path-scoped but unescaped; check the path.

**XSS / templates (Edge templating)**
- Edge auto-escapes `{{ user }}`; `{{{ user }}}` (triple-brace) is raw.
- `@!brace{ user }` / `@brace{{{ user }}}` directives — bypass escape.

**Auth**
- `@adonisjs/auth` with `driver: 'session'` + hardcoded `appKey` — `config/app.ts` `appKey` must be env-sourced, not literal.  Leak = forged sessions.
- API tokens: `auth.use('api').authenticate()` — token validation uses DB lookup; stale tokens after logout require explicit revocation logic.
- `auth.user?.id` trusted without re-fetching roles each request → stale-role issue.

**CSRF**
- `@adonisjs/shield` CSRF middleware enabled via config.  `Routes.group().middleware('csrf')` on specific scopes; routes outside lack CSRF protection.
- `shield.csrf.enabled: false` in config — finding unless API-only.

**CORS**
- `config/cors.ts` with `origin: '*'` + `credentials: true` — credentialed wildcard.
- `origin: true` — reflects any origin.

**Environment**
- `.env` committed — finding.
- `config/database.ts` with plaintext passwords — use `Env.get`.

## Tree-sitter seeds (typescript, Adonis-focused)

```scheme
; Route DSL: Route.get / .post / .group / .middleware
(call_expression
  function: (member_expression
    object: (identifier) @r
    property: (property_identifier) @m)
  (#eq? @r "Route")
  (#match? @m "^(get|post|put|delete|patch|options|head|any|group|resource|middleware)$"))

; request.<source>
(call_expression
  function: (member_expression
    object: (identifier) @req
    property: (property_identifier) @m)
  (#eq? @req "request")
  (#match? @m "^(input|qs|body|param|header|cookie|file|only|except|validateUsing|all)$"))
```
