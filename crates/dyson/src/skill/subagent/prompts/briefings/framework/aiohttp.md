Starting points for aiohttp (Python async) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.query`, `request.post()`, `await request.json()`, `request.match_info` (URL path), `request.headers`, `request.cookies`, `await request.multipart()`, `await request.read()`.

## Sinks

**Command execution**
- `asyncio.create_subprocess_shell(user_str)` — shell-expanded, RCE.  Use `create_subprocess_exec(bin, *args)` (no shell).
- `os.system(user)`, `subprocess.run(user, shell=True)` in a handler.

**SQL (asyncpg / aiomysql / aiosqlite)**
- `await conn.execute(f"... {user}")` — f-string concat: SQLi.  Use `await conn.execute("... $1", user)` (asyncpg positional placeholders).
- `await conn.fetch(query_str_with_interp)` — same.
- SQLAlchemy async (`AsyncSession`): `session.execute(text(f"... {user}"))` — use `text("... :p").bindparams(p=user)`.

**Deserialization**
- `pickle.loads(await request.read())` in a handler — RCE.
- `yaml.load(body)` without SafeLoader.
- `msgpack.unpackb(body, raw=False)` with custom object hooks pulling attacker-named classes.

**Path / file**
- `web.FileResponse(user_path)` — traversal.
- `await request.multipart()` + `await field.filename` attacker-controlled; anchor the save path with `pathlib.Path.name` (basename) + realpath prefix check.
- Static serve: `web.static('/static', user_dir)` where `user_dir` is config-derived and ever user-writable.

**WebSocket**
- `ws.receive_json()` — attacker-controlled dict / list; downstream key-walk is the prototype-walk primitive analogue in Python.
- `ws.receive_bytes()` fed into `pickle.loads` — RCE.

**Redirect / SSRF**
- `web.HTTPFound(user_url)` / `raise web.HTTPTemporaryRedirect(user_url)` — open redirect.
- `aiohttp.ClientSession().get(user_url)` — SSRF; no default host allowlist.  `ssl=False` disables TLS cert validation, commonly toggled during debugging and committed.

**Middleware / auth**
- Missing `@web.middleware` auth function in the app factory, or the middleware not added via `app.middlewares.append(...)` for the right subrouter.
- `aiohttp_session` with `EncryptedCookieStorage` needs a 32-byte secret; a shorter key silently falls back to a weaker mode in older versions.
- `aiohttp_jinja2.setup(app, loader=...)` + `render_template` — templates auto-escape; `| safe` filter bypasses.

**CORS**
- `aiohttp_cors.setup(app, defaults={"*": aiohttp_cors.ResourceOptions(allow_credentials=True, allow_origins=["*"])})` — credentialed wildcard CORS.

**Crypto**
- `hashlib.md5` / `sha1` for password hashing — use PBKDF2 / Argon2.
- `secrets` module is the right source for tokens; `random` is not.

## Tree-sitter seeds (python, aiohttp-focused)

```scheme
; Route registration: app.router.add_get / .add_post / etc.
(call function: (attribute
    object: (_)
    attribute: (identifier) @m)
  (#match? @m "^(add_get|add_post|add_put|add_delete|add_patch|add_options|add_route|add_resource)$"))

; request.<source>
(attribute
  object: (identifier) @o
  attribute: (identifier) @a
  (#eq? @o "request")
  (#match? @a "^(query|post|json|match_info|headers|cookies|multipart|rel_url)$")) @src
```
