Starting points for Starlette (Python ASGI) — not exhaustive. FastAPI is built on top of Starlette; patterns here apply to plain-Starlette apps that don't use FastAPI's typed-route + Pydantic layer. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.query_params["k"]`, `await request.json()`, `await request.form()`, `request.path_params["id"]`, `request.headers["H"]`, `request.cookies.get("c")`, `await request.body()`, `await request.stream()`.

No built-in shape validation.  Handlers must validate by hand.  A `JSONResponse(content=await request.json())` echoes raw attacker JSON.

## Sinks

**SQL (via SQLAlchemy, databases, asyncpg, etc.)**
- `await database.execute(f"SELECT ... '{user}'")` — f-string SQLi.
- `session.execute(text(f"... {user}"))` — use `text("... :p").bindparams(p=user)`.

**Command execution**
- `subprocess.run(user, shell=True)` in a handler — RCE.
- `os.system(user)`.

**Redirect / SSRF**
- `RedirectResponse(url=user_url)` — open redirect unless validated.
- `httpx.AsyncClient().get(user_url)` — SSRF; no default host allowlist.

**File / path**
- `FileResponse(user_path)` — traversal.
- `StaticFiles(directory=user_dir)` at app construction — attacker-derived serve root.

**XSS / templates**
- `HTMLResponse(content=user_html)` — raw HTML body.
- Jinja2Templates: `{{ user | safe }}`, `{% autoescape false %}` — bypass.

**Auth middleware**
- Custom `AuthenticationBackend.authenticate(self, conn)` returning `(AuthCredentials(['authenticated']), SimpleUser(conn.cookies.get('uid')))` without verifying signing cookie — forged identity.
- `AuthenticationMiddleware` registered but specific routes mount their own handler outside the app's middleware stack — bypass.
- Missing `requires("authenticated")` decorator on state-changing routes.

**CORSMiddleware**
- `CORSMiddleware(app, allow_origins=["*"], allow_credentials=True)` — credentialed wildcard; browsers reject but misconfigured regex mode passes.

**Sessions**
- `SessionMiddleware(app, secret_key="dev")` — hardcoded session signing key.
- Starlette sessions are signed client-side cookies; no server-side store.  A leaked `secret_key` = forged sessions.

**WebSocket**
- `@app.websocket_route('/ws')` handlers receive `websocket: WebSocket` — `await websocket.receive_json()` is attacker-controlled.  Feeding the dict into downstream key walks is prototype-walk analogue.
- No auth guard by default — WebSocket clients are anonymous unless middleware enforces.

**Background tasks**
- `BackgroundTasks().add_task(fn, user_arg)` — `fn` as a callable from user input = RCE.

**Request body parsing**
- `await request.form()` parses multipart by default with `python-multipart`; no size limit by default.  DoS-adjacent (out of scope unless downstream).
- `await request.json()` uses `json.loads` — JSON itself is safe; the walk over the result is where bugs live.

**Deserialization**
- `pickle.loads(await request.body())` — RCE.
- `msgpack.unpackb(body, raw=False)` with custom object hooks for attacker-named classes — type injection.

## Tree-sitter seeds (python, Starlette-focused)

```scheme
; Route decorators: @app.route / @app.websocket_route / @router.get
(decorator (call
  function: (attribute
    object: (_)
    attribute: (identifier) @m)
  (#match? @m "^(route|websocket_route|get|post|put|delete|patch|options|head|head)$")))

; Response classes
(call function: (identifier) @f
  (#match? @f "^(JSONResponse|HTMLResponse|PlainTextResponse|FileResponse|StreamingResponse|RedirectResponse|Response)$"))

; request.<source>
(call function: (attribute
    object: (identifier) @obj
    attribute: (identifier) @m)
  (#eq? @obj "request")
  (#match? @m "^(json|form|body|stream)$"))
```
