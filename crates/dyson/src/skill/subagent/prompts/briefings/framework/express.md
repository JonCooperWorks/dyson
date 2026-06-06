Starting points for Express — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`req.query`, `req.body`, `req.params`, `req.cookies`, `req.signedCookies`, `req.headers`, `req.ip`, `req.files` (multer).

Every value in `req.query` is a string OR ARRAY OR OBJECT when `extended: true` query parser is used — `req.query.user` can be `{ $gt: "" }`, which is how NoSQL operator injection lands. Typed handlers assuming `string` get `object` at runtime.

## Sinks

**Redirect**
- `res.redirect(req.query.next)` — open redirect unless allowlisted.
- `res.redirect(301, req.body.url)`.

**Path / file**
- `res.sendFile(req.params.file)` — traversal. Use `res.sendFile(path.resolve(base, path.basename(req.params.file)))`, and verify `resolved.startsWith(base + path.sep)`.
- `express.static(root, { fallthrough: true })` serving `..` if the mount point is computed from user input.
- `fs.readFile(req.query.p, ...)`, `fs.createReadStream(req.query.p)` — traversal.

**Template / XSS**
- `res.render('v', { user: req.body.html })` where the template interpolates `user` unescaped (`{{{user}}}` Handlebars, `<%- user %>` EJS, `!{user}` Pug).
- `res.send(req.body.html)` with default `Content-Type: text/html` on string body — raw HTML.

**Eval / command**
- `eval(req.body.code)`, `new Function(req.body.code)()` — RCE.
- `child_process.exec(\`grep ${req.query.q} file\`)` / `execSync` — shell injection.
- `spawn('bash', ['-c', req.query.cmd])` — RCE.

**SQL / NoSQL**
- Templated SQL: `db.query(\`SELECT * FROM u WHERE id = ${req.params.id}\`)` — SQLi.
- `mysql.query("... " + req.body.x)` — SQLi.
- Mongoose: `User.find(req.body)` — operator injection (`{ username: { $gt: "" }, password: { $gt: "" } }` → arbitrary login).
- Mongoose: `User.find({ $where: req.body.q })` → JS eval server-side.
- Mongoose: `User.findOne({ username: req.body.user })` — if `req.body.user` is `{ $ne: null }` on unsanitised body, matches any user.

**JWT**
- `jsonwebtoken.verify(token, secret)` with no `algorithms` option — HS/RS confusion attack.
- `algorithms: ['none']` — explicit bypass.
- `algorithms: ['HS256', 'RS256']` — still confusion; pick ONE family.
- Hardcoded `secret = 'secret'` / `'dev'`.

**Prototype pollution → RCE**
- `Object.assign({}, req.body)` / `_.merge(config, req.body)` / `_.defaultsDeep(opts, JSON.parse(req.body))` — keys like `__proto__`, `constructor.prototype` pollute globals; downstream `res.render` options, template helpers, or `JSON.parse` reviver gadgets flip to attacker code.

**Middleware gaps (check at app bootstrap)**
- No `helmet()` — missing security headers.
- No `csurf` / CSRF token on state-changing routes that rely on session cookies.
- `cors({ origin: true, credentials: true })` — reflects any origin = credentialed CORS for everyone.
- `cookie-parser` with `secret` hardcoded.
- `express.json({ limit: '10mb' })` absent — body size DoS (out of scope per Never Report rules, unless it yields memory corruption).

## Tree-sitter seeds (javascript / typescript)

```scheme
; res.<method>(...)
(call_expression function: (member_expression
    object: (identifier) @obj
    property: (property_identifier) @p)
  (#eq? @obj "res")
  (#match? @p "^(redirect|sendFile|render|send|json|cookie)$"))

; req.<source> member access
(member_expression
  object: (identifier) @obj
  property: (property_identifier) @p
  (#eq? @obj "req")
  (#match? @p "^(query|body|params|cookies|signedCookies|headers|files)$")) @src

; JWT verify
(call_expression function: (member_expression
    property: (property_identifier) @m)
  (#eq? @m "verify"))
```
