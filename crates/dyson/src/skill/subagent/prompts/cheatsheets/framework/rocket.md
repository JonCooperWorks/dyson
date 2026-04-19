Starting points for Rocket (Rust) — not exhaustive. Type-state routing closes a lot, but the hatches matter. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
Request guards: `&State<T>` (server-only), `Json<T>`, `Form<T>`, `String` (body), `PathBuf` / segment path, `Query<T>`, `&Request<'_>` (raw), cookies via `&CookieJar<'_>`, headers via `&HeaderMap<'_>`.

## Sinks

**Path / file**
- `NamedFile::open(user_path)` — traversal unless canonicalised + base-anchored.
- `FileServer::from(user_root)` mounted at route — if `user_root` is config-derived and ever user-writable, attacker picks serve root.
- `PathBuf` segment guard REJECTS dotfiles and `..` by default — good.  A hand-rolled `&str` segment handler + `Path::new(s)` bypasses this protection.

**SQL (sqlx / diesel / rocket_db_pools)**
- `sqlx::query(&format!("... {}", user)).execute(&pool)` — SQLi; use `query!` macro.
- diesel `.raw_sql(format!("...{}", user))` — raw SQL.

**Redirect**
- `Redirect::to(uri!(...))` with a typed URI is safe.  `Redirect::to(user_string)` — open redirect.
- `Redirect::permanent(user)`, `Redirect::temporary(user)` — same.

**XSS**
- `RawHtml(user_string)` — response wrapper with no escape; XSS.
- `content::RawHtml(format!("<div>{}</div>", user))` — same.
- Template engines via `rocket_dyn_templates`: `tera` / `handlebars` autoescape; `| safe` in tera, `{{{ }}}` in handlebars bypasses.

**Command execution**
- `std::process::Command::new(user)` — RCE.
- `Command::new("sh").args(["-c", user])` — shell RCE.
- `tokio::process::Command` — same.

**JSON / deserialization**
- `Json<Value>` handlers accept any JSON tree; downstream `value["x"]["y"]` walks are prototype-walk-equivalent in Rust.
- `#[serde(deny_unknown_fields)]` should be on any DTO that surfaces from external input — check every `Deserialize` derive in the handler-facing types.

**Fairings (Rocket's middleware)**
- Auth fairings attached to a subrouter only — routes registered at a higher scope won't see auth.  Map the route tree: every protected endpoint must be under a mount that has the auth fairing attached.
- `AdHoc::on_response(|_req, resp| { ... })` rewriting response bodies — a custom CORS / header fairing that reflects an attacker origin without allowlist is a misconfiguration.

**CORS**
- `rocket_cors::CorsOptions { allowed_origins: AllowedOrigins::all(), allow_credentials: true, ... }` — credentialed wildcard CORS.

**Crypto / config**
- `rocket.toml` / `Rocket.toml` with a hardcoded `secret_key` — signing-key leak breaks private cookie auth.  The file MUST NOT be committed in a real deployment; flag if seen.
- `rocket::Config { secret_key: SecretKey::from(&[0u8; 64]), .. }` — all-zero signing key.

**Uploads**
- `TempFile<'_>` — requests are held in temp storage; `file.persist_to(path)` with a user-derived `path` is traversal.
- `Form<TempFile>` — user-supplied content-disposition filename is attacker-controlled; use basename + anchor.

## Tree-sitter seeds (rust, Rocket-focused)

```scheme
; Route attributes: #[get("/...")] / #[post] / etc.
(attribute_item (attribute
  (identifier) @a
  (#match? @a "^(get|post|put|delete|patch|options|head|catch)$")))

; Redirect::to / RawHtml / NamedFile::open / FileServer::from
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#match? @ty "^(Redirect|NamedFile|FileServer|RawHtml)$")
  (#match? @fn "^(to|permanent|temporary|open|from)$"))
```
