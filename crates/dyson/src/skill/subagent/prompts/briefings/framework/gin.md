Starting points for Gin (Go) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`c.Query("k")`, `c.DefaultQuery(...)`, `c.PostForm(...)`, `c.Param("id")` (URL path), `c.GetHeader(...)`, `c.Cookie(...)`, `c.BindJSON(&v)` / `c.ShouldBindJSON`, `c.FormFile(...)`, raw `c.Request`.

`c.ShouldBind` dispatches based on `Content-Type` — attacker chooses.  A handler expecting JSON can be fed `application/x-www-form-urlencoded` and have its struct bound from form fields instead, occasionally matching unintended field names.

## Sinks

**SQL injection**
- `db.Exec(fmt.Sprintf("... %s", user))`, `db.Query(fmt.Sprintf(...))` — SQLi.  Use `?` placeholders: `db.Query("... WHERE id = ?", user)`.
- GORM: `.Where("name = " + user)` — SQLi; `.Where("name = ?", user)` is safe.
- GORM `.Raw("... " + user)`, `.Exec("..." + user)` — raw SQL.
- sqlx.Select / sqlx.Get with `fmt.Sprintf`.

**Command execution**
- `exec.Command(c.Query("bin"), args...)` — `bin` user-controlled = RCE.
- `exec.Command("sh", "-c", c.Query("cmd"))` — shell RCE.

**File / path**
- `c.File(c.Query("path"))` — direct file serve; traversal.
- `c.SaveUploadedFile(file, c.Query("dest"))` — attacker-chosen destination.
- `os.Open(filepath.Join(base, c.Query("name")))` — traversal; `filepath.Clean` doesn't anchor.  Enforce with `strings.HasPrefix(filepath.Clean(joined), base+string(filepath.Separator))`.

**Redirect / SSRF**
- `c.Redirect(http.StatusFound, c.Query("next"))` — open redirect.
- `http.Get(c.Query("url"))` / `http.DefaultClient.Do(req)` — SSRF; no default host allowlist.

**XSS / templates**
- `c.HTML(200, "...", gin.H{"user": c.Query("u")})` — template engine (html/template) auto-escapes; `gin.H{"user": template.HTML(user_raw)}` with `template.HTML(user)` bypasses escape.
- `c.Data(200, "text/html", []byte(userHTML))` — raw HTML body; XSS.
- Using `text/template` for HTML output — no escape at all.

**Deserialization**
- `c.BindJSON(&m)` where `m` is `map[string]interface{}` or a struct with `json:",inline"` + attacker-controlled keys — untyped tree, prototype-walk-equivalent via `m[user_key]`.
- `gob.NewDecoder(c.Request.Body).Decode(&out)` — type confusion on untrusted bytes.
- `yaml.Unmarshal(body, &out)` (gopkg.in/yaml.v2) — mass-assignment into tagged structs.

**Auth / middleware**
- Routes not inside a `router.Group` that applies `auth.Middleware()` — unauthenticated; easy to miss when a new handler is added to the root router.
- `c.MustGet("user")` returning the zero value on unauthenticated requests — panic → 500 (not ideal but usually fine); the real concern is silent `c.Get("user")` returning `(nil, false)` and the handler not checking.
- CORS: `cors.Default()` in production — permissive defaults; `AllowOrigins: []string{"*"}` with `AllowCredentials: true` is invalid per spec but seen in practice.

**Rate limiting / DoS (out of scope per rules — mentioned for completeness)**
- `c.ShouldBindJSON` without a size limit: `gin.DefaultMaxMemory` is 32 MiB for multipart; JSON body size is unbounded by default.  Flag ONLY if it yields memory corruption or privilege escalation.

**JWT / auth tokens**
- `jwt.Parse(tokenString, keyFunc)` where `keyFunc` doesn't pin `token.Method.Alg()` — RS/HS confusion → forgery with the public key as HMAC secret.
- `jwt.ParseWithClaims` + `token.Valid` ignoring a specific `err` type — some libraries return `ValidationError` with a specific flag indicating signature failure; treating any error as "token expired" re-admits forged tokens.

**Context / timeout**
- `context.Background()` (not `c.Request.Context()`) in a long-running DB call — outlives the request; not a security finding alone but can hide auth-state issues.

## Tree-sitter seeds (go, Gin-focused)

```scheme
; Route registration: router.GET("/x", handler) etc.
(call_expression
  function: (selector_expression
    field: (field_identifier) @m)
  (#match? @m "^(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Any|Handle)$"))

; c.<source>()
(call_expression
  function: (selector_expression
    operand: (identifier) @c
    field: (field_identifier) @src)
  (#eq? @c "c")
  (#match? @src "^(Query|DefaultQuery|PostForm|Param|GetHeader|Cookie|BindJSON|ShouldBindJSON|ShouldBind|FormFile)$"))

; c.Redirect / c.File / c.HTML / c.Data
(call_expression
  function: (selector_expression
    operand: (identifier) @c
    field: (field_identifier) @m)
  (#eq? @c "c")
  (#match? @m "^(Redirect|File|HTML|Data|String)$"))
```
