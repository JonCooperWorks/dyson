Starting points for Bottle (Python micro-framework) — not exhaustive. Single-file framework often used for admin scripts and internal tools — the "well it's internal" justification means a lot of committed vulns. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.query.k` / `request.query.get('k')`, `request.forms.k`, `request.params` (merged), `request.json`, `request.body.read()`, `request.headers.get('H')`, `request.cookies.k`, `request.files.upload`.

## Sinks

**SQL (sqlite3 / MySQLdb / psycopg2)**
- `cursor.execute(f"SELECT ... '{user}'")` — f-string SQLi.  Use `?` / `%s` placeholders.
- SQLAlchemy text / raw concerns — same as elsewhere.

**Command execution**
- `subprocess.run(user, shell=True)` — RCE.
- `os.system(user)` — admin-panel pattern for "just run this command on the server" is an RCE waiting to happen.

**Template (SimpleTemplate / Jinja2 / Mako integrations)**
- SimpleTemplate (Bottle's default): `{{ user }}` auto-escapes; `{{! user }}` is raw.
- `template('... {{ user }}')` — if the template STRING is user-controlled, SSTI.
- `template(user_str)` — attacker chooses the template source.

**File / path**
- `static_file(user_path, root='./public/')` — Bottle checks that `user_path` doesn't escape `root`, BUT only via `os.path.abspath` + `startswith`.  A `root='./public/'` (trailing slash missing in some versions) could fail the anchor on certain OS path quirks.  Prefer absolute `root`.
- `open(user_path).read()` directly in a handler — traversal.
- `request.files.upload.save(user_dir + '/' + filename)` — attacker filename.

**Redirect**
- `redirect(user_url)` — open redirect.  Use `redirect('/path')` for internal-only.
- `response.status = 302; response.set_header('Location', user_url)` — same.

**Deserialization**
- `pickle.loads(request.body.read())` — RCE.
- `yaml.load(request.body.read())` (non-safe) — RCE.
- Session plugins (bottle-session, bottle-cork) with pickle-based storage — session-cookie forgery = RCE.

**Auth**
- `bottle.auth_basic` decorator with hardcoded username/password checker — committed credentials.
- `request.get_cookie('user', secret='dev')` — hardcoded signing secret for cookie signature; leak = forged user cookies.

**Error debug mode**
- `bottle.run(debug=True)` / `bottle.debug(True)` — detailed error pages including stack traces, local variable contents (which can contain secrets).  Never in production.
- Default dev server; for production behind WSGI is standard, but if run directly with `debug=True` exposed externally, total info leak.

## Tree-sitter seeds (python, Bottle-focused)

```scheme
; Route decorators: @route / @get / @post / @app.route
(decorator (call
  function: (identifier) @d
  (#match? @d "^(route|get|post|put|delete|patch|error|hook)$")))

; request.<source>
(attribute
  object: (identifier) @o
  attribute: (identifier) @m
  (#eq? @o "request")
  (#match? @m "^(query|forms|params|json|body|headers|cookies|files|environ)$")) @src

; redirect / static_file / template / response.*
(call function: (identifier) @f
  (#match? @f "^(redirect|static_file|template|abort)$"))
```
