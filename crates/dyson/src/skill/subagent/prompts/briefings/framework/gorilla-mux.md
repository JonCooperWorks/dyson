Starting points for gorilla/mux (Go) — not exhaustive. Pure router on top of net/http; handler bodies get stdlib `*http.Request`. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`mux.Vars(r)["id"]` (path parameters), `r.URL.Query().Get("k")`, `r.FormValue("k")`, `r.PostFormValue("k")`, `r.Header.Get("H")`, `r.Cookie("c")`, `json.NewDecoder(r.Body).Decode(&v)`, `r.MultipartReader()`.

No built-in validation — every handler decodes and validates by hand.  `decoder.DisallowUnknownFields()` is the Go idiom for rejecting unexpected JSON fields.

## Sinks

**SQL**
- `db.Exec(fmt.Sprintf("... %s", mux.Vars(r)["id"]))` — SQLi; use `?` / `$1` placeholders.
- GORM: `.Where("name = " + user)` is SQLi.

**Command execution**
- `exec.Command(r.URL.Query().Get("bin"), ...)` — RCE.
- `exec.Command("sh", "-c", userCmd)` — shell RCE.

**File / path**
- `http.ServeFile(w, r, userPath)` — traversal (see [framework/chi.md](chi.md) for the same concerns).
- `http.FileServer(http.Dir(userRoot))` — attacker-derived serve root.

**Redirect**
- `http.Redirect(w, r, userURL, http.StatusFound)` — open redirect unless host-allowlisted.

**Middleware / subrouter**
- `r.PathPrefix("/api").Subrouter().Use(authMiddleware)` — auth on subrouter.  Routes registered on `r` directly (not subrouter) bypass.  Map the route tree.
- Middleware order: `r.Use(a, b, c)` — `a` runs first.  An auth middleware registered AFTER a handler-consuming middleware means auth sees a drained body.

**gorilla/csrf**
- `csrf.Protect([]byte("dev-secret"))` — hardcoded key.
- `csrf.Secure(false)` — cookie sent over HTTP.
- `csrf.TrustedOrigins(...)` with user-controlled entries.

**gorilla/sessions**
- `sessions.NewCookieStore([]byte("dev"))` — hardcoded signing key.
- Session cookies are signed; a leaked signing key enables forgery.
- `gob`-serialized session values on a `FilesystemStore` with user-writable temp dir — writable path = attacker writes a `gob` file that gets decoded on read (gob decoding is type-unsafe on untrusted bytes).

**WebSocket (gorilla/websocket)**
- `upgrader.CheckOrigin = func(r *http.Request) bool { return true }` — accepts any origin; CSRF-equivalent for WebSocket.
- `conn.ReadMessage()` returns attacker-controlled `(messageType, message, err)`; downstream JSON-unmarshal + key walk = prototype-walk analogue.

**URL routing edge cases**
- `r.Host("{subdomain}.example.com")` with user-controlled subdomain in downstream logic (logging, cache keys).
- `r.Queries("k", "{v:.*}")` — regex constraints; broken regex can over-match.

## Tree-sitter seeds (go, gorilla/mux-focused)

```scheme
; r.HandleFunc / r.Handle / r.PathPrefix / r.Subrouter / r.Methods
(call_expression
  function: (selector_expression
    field: (field_identifier) @m)
  (#match? @m "^(HandleFunc|Handle|PathPrefix|Path|Subrouter|Methods|Headers|Queries|Host|Schemes|Use)$"))

; mux.Vars(r)
(call_expression
  function: (selector_expression
    operand: (identifier) @pkg
    field: (field_identifier) @f)
  (#eq? @pkg "mux")
  (#eq? @f "Vars"))
```
