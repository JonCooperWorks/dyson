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
- **Scope-delegation dismissal is NOT a mitigation.** When an in-scope function receives attacker-controlled input and hands it to an unsafe operation in a sibling package or module outside the review root, the wrapper is the attacker's API — file it.  Phrases to reject verbatim: "the real parser / deserializer / sink lives in another package", "wraps X which is outside scope", "delegates to Y in another module — not reviewed here".  File at the wrapper's exported entry; cite the delegation call site (the `parse(...)`, `decode(...)`, `load(...)`, `resolve(...)` etc. that receives the tainted value) as the sink line; describe the downstream unsafe op in Impact.  The wrapper being one file-move away from the sink does not exonerate it.
- **Preferred evidence for this class**: one `taint_trace` invocation from the wire-read (`FormData.get`, `request.formData()`, `req.body`, stream-chunk assembly) to the walk loop's sink line.  Same-file / same-function / same-line traces count.  If budget prevents running one, still ship the finding — cap severity per the main Severity Caps rule, do NOT downgrade to a progress-update memo.
- **Raise `max_depth` on deep dispatchers.**  Defaults are `max_depth=16, max_paths=10`.  For RSC / RPC / message-bus / reflection-heavy dispatch (anything that looks like `registry[id](args)`, `handlers[type].call(...)`, `modules[name].run(...)`), pass `max_depth: 32, max_paths: 20` to `taint_trace` — the indirection layer adds 4-8 hops that the default cap cuts short.  `[TRUNCATED]` in the index header means you still need more; 48/30 is the realistic upper bound.

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

**Trust-boundary headers — framework / runtime internal signals read from the wire**
A class of auth / authorization / middleware bypass that appears in nearly every web framework (Express, Next.js, Fastify, Hono, NestJS, tRPC, Remix, SvelteKit, Koa): the framework uses an internal header to tell its own runtime "this request is a subrequest / has already been authorized / is from a trusted origin / should skip middleware".  Such a header is safe when it only travels *within* the server (loopback, sidecar, internal proxy) but becomes a full bypass when the framework reads it from the same request object that carries attacker input, without verifying provenance.

Silhouette: `const flag = req.headers[INTERNAL_HEADER_NAME]; if (flag === SOME_STRING || flag.includes(SOME_ID)) { shortCircuit(); }` where the short-circuit skips auth / middleware / rate-limit / authorization, and no upstream layer strips `INTERNAL_HEADER_NAME` from external requests.  Header names often contain `internal`, `subrequest`, `middleware`, `origin-verified`, `admin`, `signed`, `rpc`, `rsc`, `trusted`, `skip-*`, `x-forwarded-preauth` or a framework prefix like `x-nextjs-*` / `x-remix-*`.

Grep strategy: `ast_query` for `subscript_expression` / `member_expression` where the base is `req.headers` / `request.headers` / `ctx.request.headers`, harvest the indexed header names, then `taint_trace` each header read to the nearest short-circuit return / early response / `waitUntil` / `next()`-skip.  If the header value gates a control-flow branch that bypasses a middleware/auth layer AND there is no provenance check upstream (IP allowlist, signature verify, mTLS, internal-only port), file CRITICAL.

Dismissal phrases that do NOT clear this finding: "this header is set internally by the framework" (same code path reads it from external requests), "it's only used for subrequests" (attacker can forge), "it's part of the RSC/middleware protocol" (protocols that trust client-supplied headers without signing are the bug class).

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
