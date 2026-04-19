Starting points for Axum — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`Json<T>`, `Form<T>`, `Query<T>`, `Path<T>`, `Extension<T>` (when T carries request-scoped data), `HeaderMap`, `TypedHeader<T>`, `Multipart`, `Bytes`, `String` extractor (raw body).

Extractors run before the handler body — a rejection becomes a `400`. But a handler whose extractor is `Json<Value>` accepts any JSON tree and downstream `value["x"]["y"]` walks are prototype-walk-equivalent in Rust.

## Sinks

**Redirect**
- `Redirect::to(user_url)`, `Redirect::permanent(user_url)`, `Redirect::temporary(user_url)` — open redirect unless host-allowlisted.
- Manual: `(StatusCode::FOUND, [("location", user_url)]).into_response()` — same.

**XSS**
- `Html(user_string)` — the `Html` response wrapper sets `Content-Type: text/html` and ships the body verbatim. No escaping. Use `askama::Template` / `askama_axum` with autoescape on.
- `(StatusCode::OK, [(CONTENT_TYPE, "text/html")], user_body)` — same pattern without the wrapper.

**File / path**
- `tower_http::services::ServeDir::new(user_root)` mounted at route — if `user_root` is config-derived and ever user-writable, attacker picks serve root.
- `ServeDir::new(base).append_index_html_on_directories(true)` + symlinks in `base` → traversal to any target the process can read.
- `tokio::fs::read(user_path)`, `File::open(user_path)` — traversal unless canonicalised.

**SQL**
- `sqlx::query(&format!("... {}", user)).execute(&pool)` — SQLi; use `query!` macro.
- `sea_query::Query::select().and_where(expr::raw(format!(...)))` — SQLi.

**Command execution**
- `tokio::process::Command::new(user)` — RCE.
- `tokio::process::Command::new("sh").args(["-c", user])` — RCE.
- `std::process::Command` same.

**Middleware / layer ordering**
- Auth is usually a `tower` layer: `.layer(from_fn(auth_mw))` or `RequireAuthorizationLayer::<_>::bearer(&token)`.
- `.route("/admin", ...)` registered OUTSIDE the authed router = unauthenticated. Read the router tree from the root down; every protected route must trace to an auth layer above it.
- `ServiceBuilder::new().layer(A).layer(B)` — ordering matters; a logging/tracing layer that reads the body can exhaust it before extractors. Not a security finding on its own; can mask auth failures.

**State / Extension**
- `State<Arc<Mutex<_>>>` — contention is perf, not security.
- `Extension<Sensitive>` present on a handler with no auth layer above → direct access.

**CORS**
- `tower_http::cors::CorsLayer::permissive()` — any origin; fine for dev, finding in prod factory.
- `CorsLayer::new().allow_origin(Any).allow_credentials(true)` — invalid per spec; `tower-http` panics at build, but variants exist that don't.

**JSON extraction**
- `Json<T>` returns `400` on parse failure — safe. `Json<Value>` defers schema to handler code; downstream `value["k"]` walks over attacker-named keys.

## Tree-sitter seeds (rust, Axum-focused)

```scheme
; Redirect::to / permanent / temporary
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#eq? @ty "Redirect")
  (#match? @fn "^(to|permanent|temporary)$"))

; Html(user) response wrapper
(call_expression function: (identifier) @f (#eq? @f "Html"))

; ServeDir::new / .append_index_html_on_directories
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#eq? @ty "ServeDir")
  (#eq? @fn "new"))

; Route registration — useful for mapping the handler graph
(call_expression function: (field_expression
    field: (field_identifier) @f)
  (#match? @f "^(route|nest|merge|layer|with_state)$"))
```
