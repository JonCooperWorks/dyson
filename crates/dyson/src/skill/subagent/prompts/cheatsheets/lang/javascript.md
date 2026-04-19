Starting points for JavaScript / TypeScript — not exhaustive. Novel sinks outside this list are still in scope. TypeScript types vanish at runtime; a `Req<T>` is attacker JSON until parsed against a schema — `as any`, `as unknown as X`, and most `@ts-ignore` blocks turn the type system off.

## Sinks

**Eval / dynamic code**
- `eval(x)`, `new Function(x)`, `Function(x)()`.
- `setTimeout(x, ...)`, `setInterval(x, ...)` with a STRING first arg — eval-equivalent.
- `vm.runInNewContext(code)`, `vm.runInThisContext`, `vm.Script(code).runInContext` on untrusted `code`.
- `require(user_str)` / dynamic `import(user_str)` — loads attacker-named module.

**Command execution**
- `child_process.exec(cmd)`, `execSync`, `execFile` with interpreter (`bash`, `sh`), `spawn(..., { shell: true })` — RCE if `cmd` carries user input.
- `spawn('bin', [user])` is safer than `exec` but still RCE if the bin is an interpreter (`bash`, `node`, `python`).

**Prototype-walk / reflection (RCE primitive in JS)**
- Any loop that does `obj = obj[key]` where `key` comes from `user_str.split('.')` / `.split(':')`. Segments landing on `constructor`, `__proto__`, `prototype` yield `Function` or the constructor itself — RCE via indirect `Function("...")`. The walk IS the primitive; no explicit `eval` required.
- `_.merge`, `_.set`, `_.defaultsDeep`, `Object.assign(target, JSON.parse(user))` with keys from untrusted input → prototype pollution; a polluted `Object.prototype` flips downstream gadgets (`res.render` options, `Object.keys` checks, template helper lookups).
- `Reflect.get(obj, user_key)` — same primitive.

**RSC / RPC / wire-format deserializers (high-yield prototype-walk surface)**
- React Server Components reply parsers, tRPC-style `input` decoders, MessagePack/Avro-JSON bridges, anything that takes bytes from a `FormData` / request body and reconstructs typed values by splitting a reference string and walking a chunk graph.
- Canonical shape (memorise the SILHOUETTE, then confirm with `ast_describe` + `ast_query`):
  ```
  const path = reference.split(':');    // or '.', '/', any user-supplied separator
  let value = chunks[parseInt(path[0])];
  for (let i = 1; i < path.length; i++) {
    value = value[path[i]];             // <-- the primitive
  }
  ```
- If `value` is later used as a callable (`new value(...)`, `value(args)`, `loadServerReference(value)`), or any resolved result feeds `require`/`import`/`Function`, the walk is a live RCE primitive.
- Dismissal phrases you may NOT accept for this shape: "path segments are numeric", "path came from a trusted chunk ID list", "value is a bound callable not arbitrary". None of these are a `constructor/__proto__/prototype` blocklist. Cite the blocklist lines or file it CRITICAL.
- **Preferred evidence for this class**: one `taint_trace` invocation from the wire-read (`FormData.get`, `request.formData()`, `req.body`, stream-chunk assembly) to the walk loop's sink line.  Same-file / same-function / same-line traces count.  If budget prevents running one, still ship the finding — cap severity per the main Severity Caps rule, do NOT downgrade to a progress-update memo.

**Deserialization**
- `JSON.parse(x)` — parse itself is safe; danger is what you do with the parsed tree (see prototype walk).
- `node-serialize.unserialize(user)` → RCE (IIFE payload).
- `js-yaml` pre-4.0 `yaml.load(user)` → RCE (custom tags). 4.x defaults are safe; check the version.
- `serialize-javascript` with `unsafe: true`.

**SQL / NoSQL injection**
- `db.query(\`SELECT * FROM t WHERE id = ${req.params.id}\`)` — template-literal interpolation is string concat, not parameterisation. Use `?` placeholders with `db.query(sql, [id])`.
- `sequelize.query(sql, { type: ... })` without `replacements` or `bind`.
- Mongoose `.where({ $where: user_js })`, `.find(req.body)` with operator injection (`{ user: { $gt: "" } }` → login bypass).
- Knex `.raw(\`... ${user}\`)` — SQLi.

**Templating / XSS**
- `res.send(user_html)` with no encoding + `Content-Type: text/html`.
- `element.innerHTML = user`, `document.write(user)`, `outerHTML`, `insertAdjacentHTML`.
- React `dangerouslySetInnerHTML={{ __html: user }}`.
- Handlebars `{{{user}}}` (triple-brace = no escape), EJS `<%- user %>` (dash = no escape), Pug `!{user}`.
- `res.render('view', { html: user })` where the view interpolates `html` unescaped.

**Redirect / SSRF**
- `res.redirect(req.query.next)` without allowlist → open redirect.
- `axios.get(user_url)`, `fetch(user_url)`, `http.request({ host: user })` without host allowlist → SSRF. Node fetch follows redirects by default.

**JWT / crypto**
- `jwt.verify(token, secret)` without `algorithms` option — RS256 / HS256 confusion (sign with pub key as HMAC secret = forgery).
- `jwt.verify(token, secret, { algorithms: ['none'] })` — explicit bypass.
- `crypto.createHash('md5')` / `'sha1'` for password hashing.

## Tree-sitter seeds (javascript / typescript)

```scheme
; eval / Function
(call_expression function: (identifier) @f (#match? @f "^(eval)$"))
(new_expression constructor: (identifier) @c (#eq? @c "Function"))

; child_process.exec / execSync
(call_expression function: (member_expression
    property: (property_identifier) @m) @c
  (#match? @m "^(exec|execSync|execFile|spawn|spawnSync|fork)$"))

; res.<sink>(...) — express-style
(call_expression function: (member_expression
    object: (identifier) @obj
    property: (property_identifier) @p)
  (#eq? @obj "res")
  (#match? @p "^(redirect|sendFile|render|send)$"))

; dynamic import / require
(call_expression function: (identifier) @f (#eq? @f "require"))
(call_expression function: (import)) ; dynamic import(x)
```
