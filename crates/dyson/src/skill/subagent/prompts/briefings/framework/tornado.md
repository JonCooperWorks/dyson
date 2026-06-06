Starting points for Tornado (Python async) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`self.get_argument("k")`, `self.get_arguments("k")`, `self.get_query_argument(...)`, `self.get_body_argument(...)`, `self.request.body` (raw bytes), `self.request.headers`, `self.request.cookies`, `self.path_args` / `self.path_kwargs` (URL segments).

## Sinks

**SQL (aiomysql / asyncpg / sqlalchemy)**
- `await conn.execute(f"... {user}")` — f-string: SQLi.  Use `?` / `$1` placeholders.
- `tornado_mysql.Pool().execute("... %s", user)` — `%s` is parameterised; `% (user,)` is string formatting and IS SQLi.

**Command execution**
- `tornado.process.Subprocess(user_str, shell=True)` — shell RCE.  Array form + `shell=False` is safer.
- `os.system(user)` / `subprocess.run(user, shell=True)` inside a handler.

**Template injection (Tornado templates)**
- `self.render_string(user_template, **ctx)` — SSTI if `user_template` is attacker-controlled.
- Templates auto-escape by default.  `{% raw user %}` / `{% autoescape None %}` bypass.  User input concatenated into `{{ %s % user }}` = template execution.

**Deserialization**
- `pickle.loads(self.request.body)` — RCE.
- `tornado.escape.json_decode(body)` is safe itself; downstream walks over the result are prototype-walk territory.

**Path / file**
- `tornado.web.StaticFileHandler.get_absolute_path(root, user_path)` — traversal if `root` isn't anchored against the computed path.  Built-in subclasses do the check; custom overrides can regress.
- `self.write(open(user_path).read())` — direct disclosure + traversal.

**Redirect / SSRF**
- `self.redirect(user_url)` — open redirect unless validated.
- `tornado.httpclient.AsyncHTTPClient().fetch(user_url)` — SSRF; no default host allowlist.

**Cookies / XSRF**
- `self.check_xsrf_cookie()` is called automatically for POST/PUT/DELETE when `xsrf_cookies=True` in the app settings.  App without this flag has no CSRF protection for cookie-authed handlers.
- `self.set_cookie("name", value, secure=False, httponly=False)` for session data — leaks over HTTP.
- `self.set_signed_cookie` / `get_signed_cookie` rely on `cookie_secret`; a hardcoded / committed secret breaks session integrity.

**WebSocket**
- `WebSocketHandler.on_message(self, message)` — `message` is attacker-controlled bytes/text.  Feeding into `json.loads` + walking keys = prototype-walk primitive analogue.
- Missing `check_origin` override — default accepts any origin; CSRF-equivalent for WebSockets.

**Authentication**
- `@tornado.web.authenticated` decorator requires `get_current_user()` to return truthy.  A `get_current_user` that returns a bool from a user-supplied cookie without signing check = forged auth.
- `login_url` in app settings is just a redirect target; doesn't authenticate.

## Tree-sitter seeds (python, Tornado-focused)

```scheme
; RequestHandler methods: get / post / put / delete
(function_definition
  name: (identifier) @m
  (#match? @m "^(get|post|put|delete|patch|options|head|prepare|on_message)$"))

; self.get_* / self.request.*
(call function: (attribute
    object: (identifier) @obj
    attribute: (identifier) @m)
  (#eq? @obj "self")
  (#match? @m "^(get_argument|get_arguments|get_query_argument|get_body_argument|get_cookie|get_signed_cookie|write|render|render_string|redirect|set_cookie|set_signed_cookie)$"))
```
