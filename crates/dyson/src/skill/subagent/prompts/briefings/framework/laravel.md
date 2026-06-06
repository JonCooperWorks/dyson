Starting points for Laravel — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`$request->input('k')`, `$request->get('k')`, `$request->all()`, `$request->json()`, `$request->file('f')`, `$request->header('H')`, `$request->cookie('c')`, route parameters (`Route::get('/{id}', fn($id) => ...)`), `Request::input`.

## Sinks

**SQL injection (Eloquent / DB escape hatches)**
- `DB::raw("... $user")`, `DB::select("SELECT ... '$user'")` — raw SQL with interpolation.  Use `DB::select("... ?", [$user])`.
- Eloquent `->whereRaw("name = '$user'")` / `->orderByRaw($user)` / `->havingRaw($user)` / `->groupByRaw($user)` — all raw-SQL hatches.
- `->where('name', $user)` with column concat: `->where("t.col_$user", '=', 'x')` — column-name injection.
- Schema builder: `DB::statement("ALTER TABLE users ADD COLUMN $user TEXT")` — DDL injection via config endpoints.

**Mass assignment**
- `User::create($request->all())` — without `$fillable` / `$guarded`, attacker sets any column (`admin=true`, `email_verified_at=now()`).
- `$user->fill($request->all())->save()` — same.
- Fix: `$user->fill($request->only(['name', 'email']))` or explicit `$fillable`.

**Deserialization**
- `unserialize($request->input('data'))` — PHP unserialize RCE via magic-method chains.  Absolutely never on user input.  `allowed_classes` option limits exploitation but doesn't eliminate it.
- Sessions: default driver (`file` / `database`) uses `serialize`.  Attacker who forges a session cookie (signing-key leak) gets RCE via session restore.

**Command execution**
- `exec($request->input('cmd'))`, `shell_exec`, `system`, `passthru`, backticks in a controller → RCE.
- `Artisan::call($user, $args)` — `$user` as the command name lets attackers invoke any Artisan command including `migrate:fresh` (data destruction) or `tinker` (arbitrary PHP).

**Eval / dynamic code**
- `eval($request->input('code'))` — direct RCE.
- `Blade::compileString($user)` followed by template render — SSTI.
- `view()->make($user_template_name)` — attacker-chosen template path.

**File / path**
- `Storage::get($request->input('path'))` — traversal unless `basename()` + allowlist.
- `file_get_contents($request->input('path'))` in a controller — traversal + `http://` / `phar://` stream wrappers.
- `$request->file('upload')->getClientOriginalName()` — attacker-supplied filename; `move(base, $original)` without `basename()` is traversal.  Use `store()` / `storeAs()` with a generated name.

**Template / XSS**
- Blade `{!! $user !!}` — unescaped.  `{{ $user }}` is HTML-encoded.
- `HTML::raw($user)` / `Html::obfuscate($user)` bypass.

**Authentication / authorization**
- Missing `auth` middleware on a route that needs it.
- `Gate::before(fn ($user) => true)` — grants everything; check if committed.
- Policies returning `true` unconditionally.
- `$this->authorize('update', $post)` absent in a controller that mutates `$post`.

**CSRF**
- Routes inside the `web` middleware group get CSRF via `VerifyCsrfToken`; routes in `api` (`routes/api.php`) do NOT, and use session auth via cookies is a common misconfiguration → state-changing endpoints without CSRF protection.
- `VerifyCsrfToken::$except = ['*']` — CSRF disabled; finding.

**SSRF**
- `Http::get($user_url)` / `Guzzle` without host allowlist.

**Redirect**
- `redirect()->to($request->input('next'))` — open redirect.
- `redirect()->away($user_url)` — explicit external redirect (used as a feature, but if `$user_url` is attacker-controlled with no allowlist, finding).

**Secrets in source**
- `.env` in source committed is a critical finding.  `config/app.php` with `'key' => env('APP_KEY', 'base64:...')` — the fallback literal in the second arg is a secret.
- `config/database.php` with plaintext password.

## Tree-sitter seeds (php, Laravel-focused)

```scheme
; DB::raw / Eloquent raw methods
(scoped_call_expression
  scope: (name) @c
  name: (name) @m
  (#eq? @c "DB")
  (#match? @m "^(raw|select|statement|unprepared)$"))

(member_call_expression
  name: (name) @m
  (#match? @m "^(whereRaw|orderByRaw|havingRaw|groupByRaw|selectRaw)$"))

; unserialize / eval / Artisan::call
(function_call_expression
  function: (name) @f
  (#match? @f "^(unserialize|eval|exec|shell_exec|system|passthru)$"))
```
