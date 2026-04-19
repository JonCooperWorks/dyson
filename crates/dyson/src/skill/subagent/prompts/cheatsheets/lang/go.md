Starting points for Go — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `exec.Command(user, args...)` — if `user` (the binary path) is attacker-controlled, RCE. Even with a fixed binary, passing `bash -c user_arg` or `sh -c user_arg` is RCE.
- `exec.CommandContext(ctx, user, ...)` — same concern.

**Reflection (Go's prototype-walk primitive)**
- `reflect.Value.FieldByName(user_str)` — attacker-named field reads; leaks private data across package boundaries.
- `reflect.Value.MethodByName(user_str)` — attacker-named method invocation.
- `reflect.ValueOf(obj).FieldByIndex(parsed_from_user)` — same primitive with int indices.

**SQL injection**
- `db.Query(fmt.Sprintf("... %s", user))`, `db.Exec(fmt.Sprintf("...", user))`, `db.QueryRow(...)` — always use `?` (or `$1`) placeholders: `db.Query("... WHERE id = ?", user)`.
- `gorm`: `.Where("name = " + user)` is SQLi; `.Where("name = ?", user)` is safe.
- `sqlx.Select(&out, fmt.Sprintf(...))` — same.

**Path traversal**
- `filepath.Join(base, user)` — does NOT prevent `../` escaping base. `filepath.Clean` canonicalises but doesn't anchor to `base`. Enforce with `filepath.Rel(base, joined)` returning no leading `..`, OR `strings.HasPrefix(filepath.Clean(joined), filepath.Clean(base)+string(filepath.Separator))`.
- `os.Open(user_path)`, `os.ReadFile(user_path)`, `os.Create(user_path)` — traversal unless anchored.

**Template injection / XSS**
- `html/template` autoescapes by default — safe when used correctly.
- `text/template` does NOT escape — using it to render HTML is XSS.
- `template.HTML(user_str)` bypasses escaping — the type itself is a trust assertion.
- Inside `{{ . }}` the escape is context-sensitive (attribute vs. script vs. URL). User data dropped inside `<script>{{.}}</script>` in `html/template` becomes JS-escaped, not HTML-escaped; still dangerous if inside a JS string literal.

**Deserialization**
- `encoding/gob.Decode` on untrusted bytes — type confusion, arbitrary type instantiation.
- `gopkg.in/yaml.v2` `yaml.Unmarshal` — weaker than v3; mass-assignment into tagged structs.
- `encoding/xml.Unmarshal` — XXE only if you reimplement parsing; stdlib ignores external entities, but custom decoders may not.
- `encoding/json` into `map[string]interface{}` + downstream reflection = property-walk primitive.

**SSRF**
- `http.Get(user_url)`, `http.DefaultClient.Do(req)` without host allowlist, `net.Dial("tcp", user_addr)`.
- `http.DefaultTransport` follows redirects to internal addrs by default; set `CheckRedirect`.

**Crypto**
- `crypto/md5`, `crypto/sha1` for password hashing or auth tokens.
- `math/rand` (not `crypto/rand`) for session IDs, tokens, nonces.
- `bytes.Equal(a, b)` for MAC comparison — use `hmac.Equal` / `subtle.ConstantTimeCompare`.

## Tree-sitter seeds (go)

```scheme
; exec.Command / exec.CommandContext
(call_expression function: (selector_expression
    operand: (identifier) @pkg
    field: (field_identifier) @f)
  (#eq? @pkg "exec")
  (#match? @f "^(Command|CommandContext)$"))

; db/tx.Query / Exec / QueryRow / Prepare — SQL surface
(call_expression function: (selector_expression
    field: (field_identifier) @f)
  (#match? @f "^(Query|QueryContext|QueryRow|QueryRowContext|Exec|ExecContext|Prepare)$"))

; reflect.Value.FieldByName / MethodByName
(call_expression function: (selector_expression
    field: (field_identifier) @f)
  (#match? @f "^(FieldByName|MethodByName|FieldByIndex)$"))

; filepath.Join — every hit wants a base-anchoring check
(call_expression function: (selector_expression
    operand: (identifier) @pkg
    field: (field_identifier) @f)
  (#eq? @pkg "filepath")
  (#eq? @f "Join"))
```
