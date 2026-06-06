Starting points for Flask — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.args`, `request.form`, `request.json`, `request.data`, `request.cookies`, `request.headers`, `request.files`, URL converters (`<path:p>`, `<string:s>`).

## Sinks

**Template injection (SSTI)**
- `render_template_string(user)` — arbitrary Jinja2 evaluation → RCE via `{{ ''.__class__.__mro__[1].__subclasses__() }}` gadgets.
- `flask.render_template_string(f"Hello {user}")` — same; f-string composes an attacker template.
- Jinja2 `Environment(autoescape=False)` on HTML-rendered templates → XSS.
- `{{ user|safe }}`, `{% autoescape false %}` in a template served to end users.

**Redirect**
- `redirect(request.args.get('next'))` — open redirect unless validated (`url_for` + allowlist, or `is_safe_url` from werkzeug.urls — now deprecated; roll your own host check).

**File / path**
- `send_file(user_path)`, `send_file(open(user_path, 'rb'))` — traversal unless anchored.
- `send_from_directory(base, user_name)` — better (uses `safe_join`) but `safe_join` returns None silently on an unsafe path; missing `None` check → 500 at best, path confusion at worst.
- `os.path.join(UPLOAD_DIR, request.files['f'].filename)` — attacker filename, traversal. `werkzeug.utils.secure_filename` strips most but not all edge cases.

**XSS**
- `make_response(user_html, 200)` with `Content-Type: text/html` — raw body.
- `Markup(user)` — trust assertion, bypasses Jinja autoescape.

**Command execution**
- `subprocess.run(user, shell=True)` in a view — RCE.
- `os.system(user)`, `os.popen(user)` in a view — RCE.

**SQL**
- Raw SQLAlchemy `db.session.execute(text(f"... {user}"))` — SQLi; `text("... :p").bindparams(p=user)` is safe.
- `db.engine.execute(f"... {user}")` — SQLi.
- Flask-SQLAlchemy `.filter_by(**request.args)` — mass assignment on query filters (works but can leak via unexpected filter fields).

**Auth / session**
- `app.secret_key = 'dev'` / hardcoded literal — signing bypass; session forgery.
- Session cookies are SIGNED, not ENCRYPTED by default — storing secrets in `session['x']` leaks to the client.
- Missing `@login_required` (Flask-Login) on non-public views.
- `flask_jwt_extended` with no `algorithms` argument, or `JWT_ALGORITHM = 'none'`.

**Deserialization**
- `pickle.loads(session_data)` with a custom session backend (anything non-default) → RCE on signing-key leak.
- `yaml.load(request.data)` without SafeLoader (covered in lang sheet; worth double-checking in route handlers).

**CORS**
- `flask_cors.CORS(app, origins='*', supports_credentials=True)` — any origin with credentials = CSRF-equivalent.

## Tree-sitter seeds (python, Flask-focused)

```scheme
; render_template_string / redirect / send_file / send_from_directory / make_response / Markup
(call function: (identifier) @f
  (#match? @f "^(render_template_string|redirect|send_file|send_from_directory|make_response|Markup)$"))

; request.<src>
(attribute
  object: (identifier) @obj
  attribute: (identifier) @src
  (#eq? @obj "request")
  (#match? @src "^(args|form|json|data|cookies|headers|files|values)$")) @ref
```
