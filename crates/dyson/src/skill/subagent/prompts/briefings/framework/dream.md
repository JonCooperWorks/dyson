Starting points for Dream (OCaml web framework) — not exhaustive. Thin layer on cohttp/h2; handlers are `request -> response promise`. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`Dream.query req "k"`, `Dream.param req "id"` (path), `Dream.body req` (raw), `Dream.form req` (form-encoded), `Dream.header req "H"`, `Dream.cookie req "c"`, `Dream.multipart req`.

`Dream.form` returns a promise with either valid form data or CSRF failure — `Ok of (string * string) list` / `Error`.  Handlers must pattern-match; ignoring `Error` = CSRF disabled effectively.

## Sinks

**SQL (Caqti)**
- Caqti typed queries are safe.  Raw `Caqti_request.exec Caqti_type.unit ("DELETE FROM t WHERE id = " ^ user)` — OCaml string concat: SQLi.  Use `?` placeholders with typed request builders.
- `Caqti_lwt.exec conn (Printf.sprintf "...%s" user)` — same.

**Command execution**
- `Sys.command user` / `Unix.system user` — shell; RCE.
- `Unix.execvp bin argv` — `bin` user-controlled = RCE.

**File / path**
- `Dream.send (In_channel.read_all user_path)` — traversal + direct disclosure.
- `Dream.from_filesystem "./static" user_path` — if `user_path` has `..` segments, `Dream`'s internal canonicalisation SHOULD reject, but the developer-supplied subdirectory portion must be anchored; check canonicalization.

**Redirect**
- `Dream.redirect req user_url` — open redirect unless validated.

**Template / XSS**
- Eml templates (`.eml.html`): `<%s user %>` substitutes without escape.  `<%s! user %>` with `!` is explicit raw — bypass.  Default `<%s %>` is escaped when the file extension is `.html`.
- `Dream.html user_str` — raw HTML body; no escape.

**Authentication / CSRF**
- `Dream.session_field` for per-session server state.  `Dream.invalidate_session` must be called on logout; failing to invalidate leaves the old session usable on shared devices (LOW severity typically).
- `Dream.csrf_token req` — token generation; `Dream.verify_csrf_token req token` is explicit verification.  Handlers that accept state-changing POSTs must verify, or rely on `Dream.form` which verifies automatically when CSRF field is present.
- `Dream.random n` is cryptographic; `Random.bits` is not.

**Deserialization**
- `Marshal.from_string user_str 0` — CRITICAL.  Absolute no on untrusted bytes; the native format isn't type-checked.
- `Yojson.Safe.from_string user_str` — safe.  Walking the parsed JSON with attacker-chosen keys = prototype-walk analogue.

**SSRF**
- `Cohttp_lwt_unix.Client.get (Uri.of_string user_url)` — no default host allowlist.

**Logging**
- `Dream.log "user=%s" user_value` — if `user_value` contains control characters / newlines, log-injection.  Sanitize or encode.

## Tree-sitter seeds (ocaml, Dream-focused)

```scheme
; Dream.<fn> calls
(application_expression
  (value_path
    (module_path (module_name) @mod)
    (value_name) @fn)
  (#eq? @mod "Dream")
  (#match? @fn "^(get|post|put|delete|patch|options|head|run|router|scope|middleware|redirect|html|json|send|body|form|param|query|header|cookie|from_filesystem|session_field|csrf_token|verify_csrf_token)$"))
```

OCaml's grammar is intricate; `ast_describe` on a specific snippet before writing anything more structural than the above.
