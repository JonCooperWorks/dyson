Starting points for Echo (Go) — not exhaustive. Similar shape to Gin; different middleware primitives. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`c.QueryParam("k")`, `c.QueryParams()`, `c.FormValue("k")`, `c.Param("id")` (path), `c.Request().Header.Get("H")`, `c.Cookie("c")`, `c.Bind(&v)` (content-negotiated body), `c.FormFile("f")`.

`c.Bind(&v)` dispatches by `Content-Type` — JSON / XML / form.  A JSON-expecting handler can be fed form data and bound against the same struct with attacker-chosen field values.  Use `c.Validate(v)` after `Bind` with a registered validator; untagged structs skip validation entirely.

## Sinks

**SQL injection**
- `db.Exec(fmt.Sprintf("... %s", c.Param("id")))` — SQLi.  Use parameterised `?`.
- GORM: `.Where("name = " + user)` — SQLi; `.Where("name = ?", user)` is safe.  Raw `.Raw("... " + user).Scan(&out)` is SQLi.

**Command execution**
- `exec.Command(c.QueryParam("bin"), args...)` — `bin` user-controlled = RCE.
- `exec.Command("sh", "-c", c.QueryParam("cmd"))` — shell RCE.

**File / path**
- `c.File(c.QueryParam("path"))` — direct file serve; traversal.
- `c.Attachment(c.QueryParam("path"), "download.bin")` — same traversal.
- `echo.Static("/static", c.QueryParam("dir"))` at route-register time — if `dir` is config-derived and ever user-writable, attacker picks root.

**Redirect / SSRF**
- `c.Redirect(http.StatusFound, c.QueryParam("next"))` — open redirect.
- `http.Get(c.QueryParam("url"))`, custom `http.Client.Do(req)` — SSRF.

**XSS / templates**
- `c.HTML(200, userHTML)` — raw HTML body; XSS.
- Renderer interface (`echo.Renderer`) is user-supplied; whether it escapes depends on the engine.  `html/template` auto-escapes; `text/template` does NOT.

**Deserialization**
- `c.Bind(&m)` where `m` is `map[string]interface{}` — untyped tree; downstream key walk = prototype-walk equivalent.
- `xml.Unmarshal(body, &v)` via XML binding — XXE concerns if handled at a lower layer (check for XML EntityResolver settings).

**Auth / middleware**
- `e.Group("/api", middleware.JWT(...))` — JWT middleware at group level.  A route registered directly on `e` (not the group) bypasses.  Map the route tree.
- `middleware.JWT([]byte("dev-secret"))` — hardcoded signing key.
- `middleware.JWTWithConfig{SigningMethod: "none"}` — never correct.
- `middleware.BasicAuth(func(u, p string, c echo.Context) (bool, error) { return true, nil })` — any credentials accepted.

**CORS**
- `middleware.CORS()` default — permissive dev config.
- `middleware.CORSWithConfig(middleware.CORSConfig{AllowOrigins: []string{"*"}, AllowCredentials: true})` — invalid per spec; Echo passes through.

**CSRF**
- `middleware.CSRF()` with `CookieSameSite: http.SameSiteNoneMode` + no `CookieSecure: true` — token cookie leakable over HTTP.
- Missing `middleware.CSRF()` on state-changing routes that rely on cookie-based auth.

**Error handling**
- Default HTTP error handler emits stack traces in `e.Debug = true`; leaking in production is an info-disclosure finding.
- `c.Logger().Errorf("%+v", err)` with PII in `err`.

## Tree-sitter seeds (go, Echo-focused)

```scheme
; Route registration: e.GET / .POST / .Group / etc.
(call_expression
  function: (selector_expression
    field: (field_identifier) @m)
  (#match? @m "^(GET|POST|PUT|DELETE|PATCH|OPTIONS|HEAD|Any|Group|Static|File)$"))

; c.<source / sink>
(call_expression
  function: (selector_expression
    operand: (identifier) @c
    field: (field_identifier) @m)
  (#eq? @c "c")
  (#match? @m "^(QueryParam|QueryParams|FormValue|Param|Cookie|Bind|Redirect|HTML|File|Attachment|Blob|Stream|String|JSON)$"))
```
