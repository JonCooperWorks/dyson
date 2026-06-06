Starting points for Falcon (Python API framework) — not exhaustive. Minimal, WSGI/ASGI resource-based API framework. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`req.get_param("k")`, `req.get_param_as_int`/`_as_float`/`_as_bool`/`_as_list`, `req.media` (content-negotiated body — JSON / msgpack / YAML based on `Content-Type`), `req.params` (all query params), `req.headers`, `req.cookies`, `req.bounded_stream` / `req.stream`.

`req.media` is the auto-deserialized body.  Default media handlers include JSON; msgpack / YAML / form are optional.  **`YAML` media handler uses `yaml.safe_load` but projects sometimes override with `yaml.load` → RCE.**

## Sinks

**SQL**
- `cursor.execute(f"... {req.get_param('x')}")` — SQLi; use parameterised queries.
- SQLAlchemy `session.execute(text(f"... {user}"))` — use bind params.

**Deserialization**
- `req.media` decoded by a custom media handler that calls `pickle.loads` — RCE.
- Custom `JSONHandler` replaced with `yaml.load` (non-safe) — RCE.
- `msgpack` handler with `object_hook` that instantiates classes from attacker strings — type injection.

**Command execution**
- `subprocess.run(user, shell=True)` / `os.system(user)` inside `on_get` / `on_post` etc.

**File / path**
- `resp.stream = open(user_path, 'rb')` — direct file streaming; traversal.
- `resp.text = open(user_path).read()` — same.

**Redirect**
- `raise falcon.HTTPFound(location=user_url)` — open redirect.
- `resp.location = user_url; resp.status = falcon.HTTP_302` — same.

**Auth middleware**
- Custom `process_request(req, resp)` middleware setting `req.context.user = parse_jwt(req.get_header('Authorization'))` WITHOUT signature verification — forged identity.
- `falcon.HTTPUnauthorized` raised conditionally on something trivial like `if not req.get_header('Authorization')` — presence check only, not validation.

**XSS**
- `resp.content_type = 'text/html'; resp.text = user_html` — raw HTML body.
- No built-in template engine; integrations carry their own escape concerns.

**CORS**
- `falcon-cors` with `allow_origins_list=['*']` + `allow_credentials=True` — credentialed wildcard.

**Strict routing**
- `app.add_route('/items/{id:int}', ItemResource())` — Falcon converters (`:int`, `:uuid`) reject malformed values with 404.  `{id}` without a converter accepts anything including path-encoded slashes.

**ASGI lifecycle**
- `async def process_request_async(...)` blocking operations (`requests.get()` instead of `httpx.AsyncClient().get()`) — blocks the event loop.  Not a security finding directly; can mask timing-based authorization.

**Error handlers**
- `app.add_error_handler(Exception, handler)` where `handler` emits `str(exc)` in response — leaks stack / secret content.

## Tree-sitter seeds (python, Falcon-focused)

```scheme
; Resource class method handlers: on_get / on_post / etc.
(function_definition
  name: (identifier) @m
  (#match? @m "^(on_get|on_post|on_put|on_delete|on_patch|on_options|on_head|on_get_async|on_post_async|process_request|process_response|process_resource)$"))

; req.<source> / resp.<sink>
(call function: (attribute
    object: (identifier) @o
    attribute: (identifier) @m)
  (#match? @o "^(req|resp)$")
  (#match? @m "^(get_param|get_param_as_int|get_param_as_bool|get_param_as_list|get_media|get_header|get_cookie_values|get_header_as_int|media)$"))
```
