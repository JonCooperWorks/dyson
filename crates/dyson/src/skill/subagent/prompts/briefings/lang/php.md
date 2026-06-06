Starting points for PHP — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `system($user)`, `exec($user)`, `shell_exec($user)`, backticks `` `...` ``, `passthru($user)`, `popen($user, 'r')`, `proc_open($user, ...)` — all RCE with user input.
- `pcntl_exec($path, $args)` — safer but `$path` attacker-controlled is RCE.

**Eval / dynamic code**
- `eval($user)` — arbitrary PHP execution.
- `assert($user_str)` — pre-7.2 evaluated strings.
- `create_function` — deprecated, eval-equivalent.
- `preg_replace("/.../e", $user, ...)` — deprecated `e` modifier evaluates replacement as PHP.
- `include "$user"`, `require "$user"`, `include_once`, `require_once` — file-based RCE if user controls path OR file contents (Local File Inclusion → RCE via log poisoning).

**Reflection (PHP's prototype-walk primitive)**
- Variable functions: `$f = $_GET['fn']; $f($arg);` — arbitrary function call.
- Variable methods: `$obj->{$user}()`, `call_user_func($user, ...)`, `call_user_func_array($user, $args)`.
- Variable classes: `new $user_class()` — arbitrary class instantiation.
- `ReflectionClass($user_name)->newInstance(...)`.

**Deserialization**
- `unserialize($user)` — classic PHP RCE via `__wakeup` / `__destruct` magic-method chains (POP chains).  `allowed_classes` option added in 7.0 but many deployments omit it.
- `phar://` stream wrapper — `file_exists("phar://user_upload.jpg")` triggers deserialization of metadata (pre-PHP 8.0).
- `yaml_parse($user)` (yaml PECL) — type-injection.

**SQL injection**
- `mysqli_query($conn, "... '" . $user . "'")` — use `mysqli_prepare` + `bind_param`.
- PDO `$db->query("... $user")` — use `prepare` + `execute([$user])`.
- `mysql_query` family (deprecated) — never safe with string concat.
- Doctrine `createQuery("... " . $user)`, Eloquent `DB::raw("... $user")`.

**Path / file**
- `file_get_contents($user)`, `fopen($user)`, `readfile($user)`, `include($user)` — traversal + SSRF via `http://`, `phar://`, `data://` stream wrappers.  `allow_url_fopen` / `allow_url_include` gate some of these.
- `move_uploaded_file($_FILES['f']['tmp_name'], $dest)` — `$dest` with user-controlled filename = traversal; always `basename()` the filename AND anchor to a base.
- `file_put_contents($user_path, $data)`.

**XSS / templates**
- `echo $user` in HTML context — always encode with `htmlspecialchars($user, ENT_QUOTES, 'UTF-8')`.
- Twig `{{ user | raw }}`, `{% autoescape false %}` — XSS.
- Blade `{!! $user !!}` (unescaped), `{{ $user }}` (encoded).

**Crypto / randomness**
- `rand()`, `mt_rand()` for tokens / IDs — predictable; use `random_bytes` / `random_int`.
- `md5`, `sha1` for password hashing — use `password_hash` (bcrypt/argon2).
- `hash_equals` for HMAC comparison (timing-safe) — using `===` is timing-unsafe.

**SSRF / request smuggling**
- `curl_exec` / `file_get_contents` with user URL — no default allowlist.
- `CURLOPT_FOLLOWLOCATION = true` + user URL can redirect to `file://`, `gopher://`, internal IPs.

## Tree-sitter seeds (php — falls under tree-sitter's "embedded" grammar; queries structural but grammar-specific)

```scheme
; Command execution entries
(function_call_expression
  function: (name) @f
  (#match? @f "^(system|exec|shell_exec|passthru|popen|proc_open|eval|assert|unserialize)$"))

; Variable function call: $f(...)
(function_call_expression
  function: (variable_name)) @var-call

; Variable method call: $obj->$method(...)
(member_call_expression
  name: (variable_name)) @var-method
```
