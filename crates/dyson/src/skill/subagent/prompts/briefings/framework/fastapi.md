Starting points for FastAPI — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
Endpoint parameters typed with `Query(...)`, `Path(...)`, `Body(...)`, `Form(...)`, `Header(...)`, `Cookie(...)`, `File(...)`, `UploadFile`, `Request` (raw), Pydantic-model body params.

Pydantic validation runs BEFORE the handler body — a `BaseModel` parameter rejects unknown fields ONLY with `model_config = ConfigDict(extra='forbid')` (or `class Config: extra = 'forbid'` in v1).  Default is `extra='ignore'`, which silently drops unknown fields — usually fine, but a `dict[str, Any]`-typed body or a model with `Any` field exposes the prototype-walk surface.

## Sinks

**SQL injection (SQLAlchemy + raw)**
- `db.execute(text(f"SELECT ... {user}"))` — f-string in `text()`: SQLi.  Use `text("SELECT ... :p").bindparams(p=user)`.
- `db.execute(text("... " + user))`, `connection.exec_driver_sql(f"...{user}")` — same.
- `Query.filter(User.name == user)` — safe (ORM).  `Query.filter(text(f"name = '{user}'"))` — not safe.
- Tortoise ORM `connection.execute_query_dict(f"...{user}")`.

**Deserialization via Pydantic**
- Pydantic with `Union[A, B, C]` types + untrusted discriminator — polymorphic parsing.  Use `Literal` discriminators with `Annotated[..., Discriminator(...)]`.
- `parse_obj_as(SomeModel, user_dict)` / `model_validate(user_dict)` with a schema that accepts `Any` or `dict` — attacker picks the tree.
- `pickle.loads(request.body)` in a handler — RCE (covered in lang/python.md, worth double-checking in FastAPI endpoints).

**Command execution**
- `subprocess.run(user, shell=True)` in a handler — RCE.
- `os.system(user)` in a handler.

**Eval / dynamic code**
- `eval(request.args["code"])` — direct RCE.
- Dynamic import: `importlib.import_module(user_name)` — loads attacker-named module.

**Path / file**
- `FileResponse(user_path)` — traversal.
- `open(user_path).read()` in a handler.
- `UploadFile.filename` — attacker-controlled; `Path(upload_dir) / upload.filename` without `Path(upload.filename).name` (basename) is traversal.

**SSRF**
- `httpx.get(user_url)`, `requests.get(user_url)`, `aiohttp.ClientSession().get(user_url)` — no default host allowlist.
- `urllib.request.urlopen(user_url)` — honors `file://`, `ftp://` on default Python builds.

**Redirect**
- `RedirectResponse(url=user_url)` — open redirect.
- `return {"redirect": user_url}` + client-side JS redirect — same concern, harder to spot.

**XSS / templates**
- `HTMLResponse(content=user_html)` — raw HTML.
- `Jinja2Templates(directory=...)` + `{{ user | safe }}` or `{% autoescape false %}` — XSS.

**Authentication / authorization**
- `Depends(oauth2_scheme)` vs endpoints lacking a `Depends` that enforces auth — look for missing `Depends` on state-changing routes.
- `HTTPBearer(auto_error=False)` — fails silently on missing token; downstream code must check `None` and reject.
- Custom `verify_token` dependency that returns `True` on malformed tokens.

**CORS**
- `app.add_middleware(CORSMiddleware, allow_origins=["*"], allow_credentials=True)` — credentialed CORS for all origins.  Browsers reject `*` + credentials, but `allow_origin_regex=".*"` succeeds and is equally bad.
- `allow_methods=["*"]`, `allow_headers=["*"]` with credentialed auth is over-permissive.

**Docs / debug exposure**
- `FastAPI(debug=True)` in production.
- `/docs` / `/redoc` / `/openapi.json` exposed without auth on a private API — information disclosure (endpoint surface + schemas).  Not CRITICAL, but LOW / INFORMATIONAL worth flagging.

**Background tasks / dependency caching**
- `BackgroundTasks.add_task(fn, user_arg)` — `fn` as a callable from user input (rare but seen) = RCE.
- `Depends` with `cache=True` — dependencies cached per-request; if a dependency returns user-supplied data, downstream handlers within the request see tainted cached value.

## Tree-sitter seeds (python, FastAPI-focused)

```scheme
; Decorator on route: @app.get("/...") / @router.post("/...")
(decorator (call
  function: (attribute
    object: (identifier) @obj
    attribute: (identifier) @m)
  (#match? @m "^(get|post|put|delete|patch|options|head|api_route)$")))

; text() / execute with string-concat / f-string
(call function: (identifier) @f (#eq? @f "text"))
(call function: (attribute attribute: (identifier) @m) (#match? @m "^(execute|exec_driver_sql|execute_query|execute_query_dict)$"))

; RedirectResponse / HTMLResponse / FileResponse
(call function: (identifier) @f
  (#match? @f "^(RedirectResponse|HTMLResponse|FileResponse|PlainTextResponse)$"))
```
