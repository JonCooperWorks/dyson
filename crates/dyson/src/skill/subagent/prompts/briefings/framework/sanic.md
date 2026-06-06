Starting points for Sanic (Python async) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.args` (query string), `request.form`, `request.json`, `request.files`, `request.body`, `request.cookies`, `request.headers`, `request.match_info` (URL path).

Sanic does not auto-validate request bodies. Use `pydantic` / `msgspec` / `sanic-ext` validators — untyped bodies are common.

## Sinks

**SQL**
- `await conn.execute(f"... {user}")` (asyncpg / aiomysql) — SQLi; use `$1` / `%s` placeholders.
- SQLAlchemy async `session.execute(text(f"... {user}"))` — use `text("... :p").bindparams(p=user)`.

**Command execution**
- `asyncio.create_subprocess_shell(user)` — shell RCE.  Use `create_subprocess_exec(bin, *args)`.
- `subprocess.run(user, shell=True)`.

**Redirect / SSRF**
- `sanic.response.redirect(user_url)` — open redirect.
- `aiohttp.ClientSession().get(user_url)` / `httpx.AsyncClient().get(user_url)` — SSRF.

**Template / XSS**
- Sanic has no built-in template engine; `sanic-ext` integrates Jinja2.  `| safe` / `autoescape=False` bypass escaping.
- `response.html(user_html)` / `HTTPResponse(body=user_html, content_type='text/html')` — raw HTML.

**File / path**
- `await response.file(user_path)` — traversal.
- `Sanic.static('/static', user_dir)` at app-start with `user_dir` config-derived — attacker-derived serve root.

**Auth / middleware**
- Missing `@app.middleware('request')` auth guard on routes that need it.
- `@protected` custom decorators that check `request.token` but don't verify signature — forged-token acceptance.
- JWT with `algorithms=['none']` or `algorithms` unset — signature bypass.

**Deserialization**
- `pickle.loads(request.body)` — RCE.
- `msgpack.unpackb(body)` with `raw=False` + custom `object_hook` — type injection.
- `ujson.loads(body)` is safe; downstream `data[user_key]` walks are prototype-walk.

**WebSocket (`@app.websocket`)**
- `await ws.recv()` — raw attacker-controlled frames.  Feeding into `json.loads` + object-walk is prototype-walk analogue.

**Sanic signals (`@signal(event.HTTP_ROUTING_AFTER)`)**
- Signals fire for every request; a signal handler storing attacker data globally = cross-request contamination.

**Workers / inspector**
- `app.run(inspector=True)` — enables a debugging inspector endpoint.  On a production binding, that's a serious info-disclosure + control-plane surface.

## Tree-sitter seeds (python, Sanic-focused)

```scheme
; Route decorators: @app.get / .post / .route
(decorator (call
  function: (attribute
    object: (identifier) @obj
    attribute: (identifier) @m)
  (#match? @m "^(get|post|put|delete|patch|options|head|route|websocket|middleware|listener|signal)$")))

; response.redirect / .file / .html
(call function: (attribute
    object: (_)
    attribute: (identifier) @m)
  (#match? @m "^(redirect|file|html|json|text|raw|stream)$"))
```
