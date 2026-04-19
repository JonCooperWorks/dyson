Starting points for CodeIgniter 4 (PHP) — not exhaustive. Opinionated MVC; built-in input filtering + CSRF, but escape hatches are common. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`$this->request->getGet('k')`, `$this->request->getPost('k')`, `$this->request->getPostGet('k')` (merged), `$this->request->getJSON()`, `$this->request->getVar('k')`, `$this->request->getFile('f')`, `$this->request->getHeader('H')`, `$this->request->getCookie('c')`.

`getGet($k, FILTER_SANITIZE_*)` runs PHP's filter extension — sanitizes but doesn't validate type; downstream code still needs to check.

## Sinks

**SQL (Query Builder / raw)**
- `$db->query("SELECT * FROM users WHERE name = '" . $user . "'")` — SQLi; use `$db->query("... ?", [$user])`.
- Query Builder `->where("name = '$user'")` — raw condition: SQLi.  `->where('name', $user)` is parameterised.
- `$db->simpleQuery("..." . $user)` — SQLi.
- `escapeString` is NOT sanitization — it's for building queries when prepared statements aren't an option.  Rarely correct.

**Deserialization**
- `unserialize($this->request->getPost('data'))` — PHP unserialize RCE.
- Session library default storage is `session.save_path` files with native PHP serialize — attackers who forge a session cookie get RCE.  `session.driver = redis/memcached/database` with a secure backend mitigates.

**Command execution**
- `exec($this->request->getVar('cmd'))`, `shell_exec`, `system`, backticks — RCE.

**File / path**
- `$this->response->download($path, null)` with user-controlled `$path` — traversal.
- `$file->move($targetDir, $file->getClientName())` — attacker filename; use `getRandomName()` / basename + allowlist.
- `view($userTemplate)` — template path injection; attacker picks arbitrary view files.

**Redirect**
- `return redirect()->to($userUrl)` — open redirect.
- `redirect()->back()` relies on HTTP Referer; fine for UX, not for auth decisions.

**XSS / templates**
- Views default to `esc()` auto-escaping ONLY when you call `esc($var)` or `<?= esc($var) ?>`; raw `<?= $var ?>` does NOT escape — XSS.
- `esc($var, 'raw')` explicit raw-context — bypasses escape for a specific context.
- Email / HTML email library: rendering user content without `esc()` in the template → email XSS.

**CSRF**
- `Config\Security::$csrfProtection = 'cookie'` or `'session'` — default is `cookie`.  `$security->shouldExcludeUri($uri)` against `$csrfExcludeURIs` — an attacker-visible exclude list means state-changing endpoints on those URIs lack CSRF.
- Custom `BaseController::$csrfExempt = true` on controllers handling state-changing ops — finding.

**Authentication (Shield or custom)**
- Shield routes auto-register at `auth/*` paths.  Custom auth extending `Myth\Auth` — check the filter chain covers every protected route.
- `$session->get('user_id')` trusted without re-fetching the user from the DB each request = stale-role issue after role changes.

**File / path / upload config**
- `$uploadedFile->store($dir, $filename)` with user-controlled `$filename` — traversal unless `basename()` + allowlist + realpath prefix check.
- MIME validation via `$file->getMimeType()` — trusts client-declared MIME; use `getMimeType()` to read magic bytes (`finfo_file`).

**Config-level secrets**
- `.env` with plaintext `encryption.key` / `database.default.password` — committed is a finding.

## Tree-sitter seeds (php, CodeIgniter-focused)

```scheme
; $this->request->get* / ->validate
(member_call_expression
  object: (member_access_expression)
  name: (name) @m
  (#match? @m "^(getGet|getPost|getPostGet|getJSON|getVar|getRawInput|getFile|getFiles|getHeader|getHeaderLine|getCookie)$"))

; $db->query / Query Builder
(member_call_expression
  name: (name) @m
  (#match? @m "^(query|simpleQuery|where|orWhere|like|orLike|having|orHaving|orderBy|groupBy|select)$"))
```
