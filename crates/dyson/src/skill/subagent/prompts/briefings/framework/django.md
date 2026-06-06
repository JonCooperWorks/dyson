Starting points for Django — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.GET`, `request.POST`, `request.COOKIES`, `request.META`, `request.body`, `request.FILES`, `request.headers`, URL kwargs from `path('<str:name>', ...)`.

## Sinks

**SQL injection (Django ORM escape hatches)**
- `Model.objects.extra(where=[user])`, `.extra(select={'x': user})`, `.extra(tables=[user])`, `.extra(params=[safe], where=[user_unsafe])` — all SQLi.
- `Model.objects.raw("... {}".format(user))` — SQLi; `raw("... %s", [user])` is safe (parameterised).
- `connection.cursor().execute("... {}".format(user))` — SQLi.
- `RawSQL("... {}".format(user), [])` in `.annotate()` / `.filter()` — SQLi.
- `F("field__{}__lookup".format(user))` — lookup injection; the lookup name is part of the parsed ORM path.

**XSS**
- `mark_safe(user)`, `format_html("{}", user)` where the placeholder is `{}` not `{0}` with auto-escape — verify each call.
- Template: `{{ user|safe }}`, `{% autoescape off %}...{{ user }}{% endautoescape %}`.
- `HttpResponse(user_html, content_type='text/html')` — raw response body, no template escaping.

**Redirect**
- `redirect(request.GET.get('next'))` without `django.utils.http.url_has_allowed_host_and_scheme` check → open redirect.
- `HttpResponseRedirect(user_url)` — same.

**File / path**
- `InMemoryUploadedFile.name` is attacker-supplied. `os.path.join(MEDIA_ROOT, upload.name)` without `os.path.basename` → traversal; and without a `realpath.startswith(MEDIA_ROOT)` check, `basename` alone doesn't stop `..\` on Windows.
- `FileResponse(open(user_path, 'rb'))` — traversal.
- `default_storage.save(user_name, content)` — traversal unless `default_storage.get_valid_name` is applied.

**Deserialization**
- Sessions: default `django.contrib.sessions.serializers.JSONSerializer` is safe; `PickleSerializer` is RCE on any session forgery (signing key compromise). Flag if `SESSION_SERIALIZER = '...PickleSerializer'`.
- Caches with `PICKLE` protocol (default in `LocMemCache`, `MemcachedCache`) — cache poisoning = RCE.
- `django.core.signing.loads` with `serializer=pickle` — RCE on signing-key leak.

**Auth / authz**
- `@csrf_exempt` on a state-changing view is a finding unless an alternative token check is present in the view body.
- Views lacking `LoginRequiredMixin` / `@login_required` / `PermissionRequiredMixin` when handling non-public data.
- `UserPassesTestMixin.test_func` that returns `True` on all paths, or reads from `request.GET` instead of `self.request.user`.
- `ALLOWED_HOSTS = ['*']` in production settings.

**Settings-level red flags (committed in source)**
- `DEBUG = True` in production settings file.
- `SECRET_KEY = '...'` literal — committed secret; finding even without a reached sink.
- `SECURE_SSL_REDIRECT = False` with cookies flagged `Secure`; inconsistent.
- `CSRF_COOKIE_HTTPONLY = False` on a session-backed CSRF token.

## Tree-sitter seeds (python, Django-focused)

```scheme
; Model.objects.<m>(...) where m is extra / raw / annotate / filter
(call function: (attribute
    attribute: (identifier) @m) @c
  (#match? @m "^(extra|raw|annotate)$"))

; mark_safe / format_html / RawSQL
(call function: (identifier) @f
  (#match? @f "^(mark_safe|format_html|RawSQL)$"))

; redirect / HttpResponseRedirect
(call function: (identifier) @f
  (#match? @f "^(redirect|HttpResponseRedirect)$"))

; Decorators: @csrf_exempt
(decorator (identifier) @d (#eq? @d "csrf_exempt"))
```
