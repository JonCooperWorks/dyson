Starting points for Fiber (Go) — not exhaustive. Express-inspired API on top of fasthttp (not net/http); some primitives differ. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`c.Query("k")`, `c.FormValue("k")`, `c.Params("id")`, `c.Get("H")` (header), `c.Cookies("c")`, `c.BodyParser(&v)`, `c.FormFile("f")`.

Fiber uses fasthttp; `c.Request()` returns `*fasthttp.Request`, not `*http.Request`.  Middleware from the net/http ecosystem does NOT drop in — check that third-party middleware is fasthttp-compatible.

## Sinks

**SQL**
- `db.Exec(fmt.Sprintf("... %s", c.Params("id")))` — SQLi; use `?` placeholders.
- GORM: `.Where("name = " + user)` is SQLi; `.Where("name = ?", user)` is safe.

**Command execution**
- `exec.Command(c.Query("bin"), ...)` — RCE.

**File / path**
- `c.SendFile(c.Query("path"))` — traversal.
- `c.Download(c.Query("path"), "file.bin")` — traversal.
- `app.Static("/static", userRoot)` at mount time — attacker-derived serve root.

**Redirect / SSRF**
- `c.Redirect(c.Query("next"))` — open redirect.
- `fasthttp.Do(req)` / stdlib `http.Get(userURL)` — SSRF; no default allowlist.

**Body parser**
- `c.BodyParser(&m)` where `m` is `map[string]interface{}` — untyped tree; downstream key walk = prototype-walk analogue.
- Fiber's `BodyParser` tries JSON / XML / form / multipart based on `Content-Type` — attacker picks format.  A handler expecting JSON can be fed form data that happens to match field tags.

**XSS**
- `c.Type("text/html").Send([]byte(userHTML))` — raw HTML.
- `c.Render("view", fiber.Map{"x": user})` — depends on the template engine registered; html/template escapes, text/template does NOT.

**Auth (Fiber middleware: `fiber/v2/middleware/jwt`, keyauth, basicauth)**
- `keyauth.New(keyauth.Config{KeyLookup: "header:Authorization", Validator: func(_ *fiber.Ctx, key string) (bool, error) { return true, nil }})` — any key accepted; finding.
- `jwtware.New(jwtware.Config{SigningKey: []byte("dev")})` — hardcoded key.
- `jwtware.Config{SigningMethod: "none"}` — never correct.
- Middleware registered on a subrouter only — routes registered on `app` directly (not the subrouter) bypass.

**CORS (`fiber/v2/middleware/cors`)**
- `cors.New(cors.Config{AllowOrigins: "*", AllowCredentials: true})` — credentialed wildcard; invalid per spec but library passes through.

**Session (`gofiber/session`)**
- `session.New(session.Config{KeyLookup: "cookie:session_id"})` — cookie-based session ID, opaque.  Check session store backend (memory / Redis / etc.) and credentials.
- `store.Get(c)` returning a session without validating `ctx.user` against a DB — stale-role issue.

**CSRF (`fiber/v2/middleware/csrf`)**
- Default `csrf.New()` uses `X-Csrf-Token` header + cookie.  A route without this middleware that accepts cookie-based auth = CSRF surface.

**WebSocket (`fiber/contrib/websocket`)**
- `c.Locals("user")` set by upgrade-time auth, then trusted by message handlers — fine.  A handler reading `c.Query("user")` from the initial WebSocket upgrade URL as identity = trivial impersonation.

## Tree-sitter seeds (go, Fiber-focused)

```scheme
; Route DSL: app.Get / Post / Group / etc.
(call_expression
  function: (selector_expression
    field: (field_identifier) @m)
  (#match? @m "^(Get|Post|Put|Delete|Patch|Options|Head|All|Use|Group|Static|Add|Mount|Route)$"))

; c.<source / sink>
(call_expression
  function: (selector_expression
    operand: (identifier) @c
    field: (field_identifier) @m)
  (#eq? @c "c")
  (#match? @m "^(Query|QueryParser|FormValue|Params|ParamsInt|Get|Cookies|BodyParser|Body|FormFile|MultipartForm|Redirect|SendFile|Download|Send|SendString|Type|Render)$"))
```
