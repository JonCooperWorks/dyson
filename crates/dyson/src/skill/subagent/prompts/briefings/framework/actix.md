Starting points for Actix-web — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`web::Json<T>`, `web::Form<T>`, `web::Query<T>`, `web::Path<T>`, `web::Bytes`, `HttpRequest::headers()`, `HttpRequest::cookie(name)`, `HttpRequest::match_info()`.

Extractors run serde against the request — a `web::Json<Value>` handler is a handler over arbitrary attacker JSON with no schema. Prefer typed structs with `#[serde(deny_unknown_fields)]`.

## Sinks

**Redirect**
- `HttpResponse::Found().append_header(("Location", user_url)).finish()` — open redirect unless host-allowlisted.
- `HttpResponse::MovedPermanently().append_header(("Location", user)).finish()` — same.
- `Redirect::to(user_url)` (via `actix-web-lab`) — same.

**File / path**
- `NamedFile::open(user_path)` — traversal unless canonicalised + base-anchored.
- `Files::new("/", user_root)` at mount time — if `user_root` is derived from config that's ever user-writable, attacker picks the serve root.
- `actix_files::Files` with `show_files_listing()` enabled + `use_hidden_files` — directory enumeration + dotfile exposure.

**XSS**
- `HttpResponse::Ok().content_type("text/html").body(user_str)` — raw body, no escaping. Use a template engine (`askama`, `tera`) with autoescape on.
- `HttpResponse::Ok().content_type("text/html; charset=utf-8").body(format!("<div>{}</div>", user))` — same.

**SQL**
- `sqlx::query(&format!("... {}", user)).execute(&pool)` — SQLi; the `query!` / `query_as!` MACROS are compile-time-safe and the fix.
- `sqlx::query_scalar(&format!(...))` — SQLi.

**Command execution**
- `std::process::Command::new(user_bin)` — RCE.
- `Command::new("sh").args(["-c", user])` — RCE.
- `tokio::process::Command` — same.

**Auth / authz**
- Actix has no built-in auth. Usually `actix-web-httpauth::middleware::HttpAuthentication::bearer(validator)` wraps a scope. Look for:
  - Routes registered outside the guarded scope (indentation / config-fn bugs).
  - `HttpAuthentication::basic(...)` with a validator that returns `Ok(req)` on any credential.
  - JWT validators that don't pin `alg` (see lang/rust.md for crypto gotchas).

**CORS**
- `actix_cors::Cors::permissive()` — allows any origin + credentials. Fine for dev; a finding when mounted on a prod app factory.
- `Cors::default().allow_any_origin().supports_credentials()` — credentialed CORS for all origins.

**Deserialization via extractors**
- `web::Json<Value>` / `web::Form<HashMap<String, Value>>` handlers that walk keys dynamically — prototype-walk-equivalent primitive in Rust (see lang/rust.md).

## Tree-sitter seeds (rust, Actix-focused)

```scheme
; HttpResponse::<ctor>().append_header / .body / .content_type
(call_expression function: (field_expression
    field: (field_identifier) @f)
  (#match? @f "^(append_header|body|content_type|insert_header)$"))

; NamedFile::open, Files::new
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#match? @ty "^(NamedFile|Files)$")
  (#match? @fn "^(open|new)$"))

; Cors::permissive / .allow_any_origin
(call_expression function: (field_expression
    field: (field_identifier) @f)
  (#match? @f "^(permissive|allow_any_origin|supports_credentials)$"))
```
