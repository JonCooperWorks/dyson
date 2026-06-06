Starting points for Pyramid (Python web framework) — not exhaustive. Config-driven routing + explicit URL dispatch, or traversal-based routing for tree-shaped apps. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.params["k"]`, `request.POST["k"]`, `request.GET["k"]`, `request.json_body`, `request.matchdict["id"]` (route params), `request.headers["H"]`, `request.cookies["c"]`, `request.body`.

`request.params` merges GET+POST; order matters for same-name keys.

## Sinks

**SQL (SQLAlchemy)**
- `dbsession.execute(text(f"SELECT ... {user}"))` — f-string: SQLi.  Use `text("... :p").bindparams(p=user)`.
- `dbsession.query(User).filter(f"name = '{user}'")` — raw where: SQLi; ORM filter with `User.name == user` is safe.

**Deserialization**
- `pickle.loads(request.body)` — RCE.
- Session: `pyramid.session.UnencryptedCookieSessionFactoryConfig(secret="dev")` — default unencrypted (signed) cookies, `secret` leak = forged session.  `SignedCookieSessionFactoryConfig` is same principle.
- `pyramid_beaker` with `pickle` backend — cache poisoning → RCE.

**Command execution**
- `subprocess.run(user, shell=True)` in a view.
- `os.system(user)` / `os.popen(user)`.

**Template / XSS**
- Chameleon / Mako / Jinja2 templates: each has its own escape-bypass idiom.
- Mako: `${user | n}` is raw (no escape); `${user}` is HTML-escaped when `default_filters=['h']` is set, but Pyramid's default Mako config may NOT set that.
- Chameleon: `${user}` auto-escaped; `${structure: user}` raw.

**Redirect**
- `HTTPFound(location=user_url)` — open redirect.
- `request.invoke_subrequest` with a user-derived path — can reach internal-only views.

**File / path**
- `FileResponse(user_path)` — traversal.
- `static_view('/static', user_dir)` at config time — attacker-derived root.

**Views & authorization**
- `@view_config(permission=NO_PERMISSION_REQUIRED)` — explicit public access.  Check state-changing views don't use this.
- `@view_config()` without `permission=...` — inherits the ACL default; may be permissive.
- `request.has_permission('edit', context)` used as a guard — the `context` object must be the actual resource, not a class; checking against a class grants the permission if any instance would be grantable.

**CSRF**
- Pyramid has `pyramid.csrf.CheckCSRFTokensPredicate`; view configs should include `check_csrf=True` or use `default_csrf_check_set` on the config.  Missing = no CSRF.
- `@view_config(require_csrf=False)` — explicit bypass; state-changing routes without an alternative token mechanism are vulnerable.

**Traversal routing**
- Pyramid traversal lets the URL `/a/b/c` resolve by walking `resource['a']['b']['c']` — the `__getitem__` implementations of resource classes MUST reject unknown keys; otherwise attacker-picked paths reach unintended resources.
- Factory-returning views: `context = factory(request)` — if the factory uses user input without authorization check, every reachable context is granted.

**Authentication**
- `AuthTktAuthenticationPolicy(secret="dev")` — hardcoded signing secret.
- `RemoteUserAuthenticationPolicy` trusts `REMOTE_USER` from the WSGI env — if deployed behind a proxy that forwards a user header, the client can spoof it unless the proxy strips/sets it authoritatively.

**Deserialization via renderers**
- `@view_config(renderer='json')` — auto-renders the view's return as JSON; safe.
- Custom renderers using `pickle` — RCE if the renderer deserializes input (rare).

## Tree-sitter seeds (python, Pyramid-focused)

```scheme
; @view_config / @notfound_view_config / @exception_view_config
(decorator (call
  function: (identifier) @d
  (#match? @d "^(view_config|notfound_view_config|forbidden_view_config|exception_view_config)$")))

; request.<source>
(attribute
  object: (identifier) @o
  attribute: (identifier) @m
  (#eq? @o "request")
  (#match? @m "^(params|POST|GET|json_body|matchdict|headers|cookies|body|has_permission)$")) @src
```
