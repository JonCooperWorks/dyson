Starting points for chi (Go) — not exhaustive. Lightweight router on top of `net/http`; inherits stdlib request primitives, so [lang/go.md](../lang/go.md) applies to the handler bodies. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`chi.URLParam(r, "id")` (path parameters), `r.URL.Query().Get("k")`, `r.FormValue("k")`, `r.Header.Get("H")`, `r.Cookie("c")`, `json.NewDecoder(r.Body).Decode(&v)`, `r.MultipartReader()`.

chi has no built-in schema validation — every handler must validate input itself.  `json.Decode(&v)` into a typed struct ignores unknown fields by default; `decoder.DisallowUnknownFields()` rejects them (use it).

## Sinks

**SQL injection**
- `db.Exec(fmt.Sprintf("... %s", chi.URLParam(r, "id")))` — SQLi.  Use `?` placeholders.
- GORM / sqlx / sqlc: the usual raw-SQL hatches — see [framework/gin.md](gin.md) for identical patterns in Go.

**Command execution**
- `exec.Command(r.URL.Query().Get("bin"), args...)` — RCE.
- `exec.Command("sh", "-c", userCmd)` — shell RCE.

**File / path**
- `http.ServeFile(w, r, userPath)` — traversal.  The stdlib helper does NOT anchor to a base; callers must pre-validate.  `http.ServeFile` does strip `..` segments BUT not every edge case (symlinks, encoded slashes in some configurations).
- `http.FileServer(http.Dir(userRoot))` — if `userRoot` is config-derived and ever user-writable, attacker picks the serve root.  `http.Dir` does NOT prevent symlink escapes.

**Redirect**
- `http.Redirect(w, r, userURL, http.StatusFound)` — open redirect unless validated.
- Always allowlist hosts; `url.Parse(userURL)` + check `u.Host` against an allowlist.

**Middleware ordering**
- chi uses `r.Use(mw)` at router or sub-router scope.  A route registered on the parent router BEFORE `r.Use(auth)` is called on a child is unaffected.  Read the route tree and map middleware to the routes it covers.
- `middleware.StripSlashes` / `middleware.Compress` are benign; auth middleware (e.g., from `go-chi/jwtauth`) must wrap protected routes.
- `r.With(auth).Get("/admin", handler)` is the explicit-per-route form — simpler to audit.

**Auth / JWT (`go-chi/jwtauth`)**
- `jwtauth.New("HS256", []byte("dev"), nil)` — hardcoded key.
- `jwtauth.Verifier(...)` placed at the wrong level — routes registered outside its subrouter pass no verification.
- `jwtauth.Authenticator` present but the app-level `jwtauth.Verifier` missing — the authenticator gets `nil` claims and passes them through as anonymous.

**CORS (typically via `go-chi/cors` or `rs/cors`)**
- `cors.AllowAll()` — `*` origin + `*` methods; credentialed endpoints break the spec's rules but the library passes through.
- `AllowedOrigins: []string{"*"}, AllowCredentials: true` — same.

**Rendering**
- `w.Write([]byte(userHTML))` after `w.Header().Set("Content-Type", "text/html")` — XSS.
- `render.HTML(w, r, userHTML)` via `go-chi/render` — raw HTML pass-through.
- `render.JSON(w, r, v)` where `v` contains user-supplied HTML — no escaping needed for JSON body, but consumers that interpret the string as HTML downstream is where it bites.

**Context leakage**
- `r.Context()` carries per-request values.  Middleware storing sensitive data in the context that a logging middleware later serialises = info disclosure.

## Tree-sitter seeds (go, chi-focused)

```scheme
; Route registration: r.Get / r.Post / etc., and .With / .Use
(call_expression
  function: (selector_expression
    field: (field_identifier) @m)
  (#match? @m "^(Get|Post|Put|Delete|Patch|Options|Head|Method|Mount|Handle|Group|Route|With|Use)$"))

; chi.URLParam / chi.RouteContext
(call_expression
  function: (selector_expression
    operand: (identifier) @pkg
    field: (field_identifier) @m)
  (#eq? @pkg "chi")
  (#match? @m "^(URLParam|URLParamFromCtx|RouteContext|RoutePattern)$"))

; http.Redirect / http.ServeFile
(call_expression
  function: (selector_expression
    operand: (identifier) @pkg
    field: (field_identifier) @m)
  (#eq? @pkg "http")
  (#match? @m "^(Redirect|ServeFile|ServeContent|FileServer)$"))
```
