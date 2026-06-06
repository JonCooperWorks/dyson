Starting points for warp (Rust) — not exhaustive. Filter-combinator framework; auth + routing compose via `.and(...)`. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`warp::query::<MyQuery>()`, `warp::body::json()`, `warp::body::form()`, `warp::path::param()`, `warp::header("h")`, `warp::cookie::cookie("c")`, `warp::multipart::form()`.

Typed filters enforce shape via `serde::Deserialize`.  `warp::body::bytes()` / `warp::body::json()` into `Value` keeps the tree untyped — prototype-walk territory.

## Sinks

**SQL (sqlx / diesel / deadpool-postgres)**
- `sqlx::query(&format!("... {}", user)).execute(&pool)` — SQLi; use `query!` macro.
- Handlers that `format!()` SQL inside a filter's `.and_then(...)` closure — the same risk at a different callsite.

**Command execution**
- `std::process::Command::new(user)` — RCE.
- `Command::new("sh").args(["-c", user])` — shell RCE.

**Path / file**
- `warp::fs::dir(user_root)` mounted via `.or(...)` — attacker-derived serve root.
- `warp::fs::file(user_path)` — traversal unless canonicalised.
- `tokio::fs::read(user_path)` inside a filter.

**Redirect**
- `Ok(warp::reply::with_header(warp::reply(), "location", user_url))` + `.status(StatusCode::FOUND)` — open redirect.

**XSS / response bodies**
- `warp::reply::html(user_string)` — raw HTML body.
- `Response::builder().header("content-type", "text/html").body(user)` — same.

**Filter composition & auth**
- `auth_filter().and(mutation_filter())` — auth applies.  A route NOT wrapped in `.and(auth_filter())` is unauthenticated.  Because routes compose via `.or()` at the top level, an unauthed branch can coexist with authed ones; mapping the filter tree is the audit.
- `auth_filter` returning a user without verifying a signing cookie / JWT signature — forged identity.
- `warp::filters::addr::remote()` used as identity → trivially spoofable in most deploys.

**CORS (`warp::cors()`)**
- `warp::cors().allow_any_origin().allow_credentials(true)` — credentialed wildcard.
- `allow_headers(vec!["authorization"])` + `allow_credentials(true)` + `allow_origin("*")` — warp's runtime checks some of these combos; verify the CORS settings against the browser spec.

**JSON body size**
- `warp::body::content_length_limit(N)` must be chained before `warp::body::json()` — absence of the limit filter leaves the default (no cap) on some versions.  DoS (out of scope unless it yields memory corruption).

**Error mapping**
- Custom `Rejection` → `Reply` handlers that include PII from the rejection cause in the body.
- `warp::reject::custom(MyErr(secret))` with a Debug impl that prints the secret.

## Tree-sitter seeds (rust, warp-focused)

```scheme
; warp::<filter>::<fn>() calls
(call_expression function: (scoped_identifier
    path: (scoped_identifier
      path: (identifier) @ns
      name: (identifier) @mod)
    name: (identifier) @fn)
  (#eq? @ns "warp")
  (#match? @mod "^(path|body|query|filters|fs|header|cookie|multipart|addr|cors)$"))

; .and / .or / .and_then / .map combinators
(call_expression function: (field_expression
    field: (field_identifier) @f)
  (#match? @f "^(and|or|and_then|or_else|map|untuple_one|boxed)$"))
```
