Starting points for Rust — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `std::process::Command::new(user)` — binary path from user → RCE.
- `Command::new("sh").args(["-c", user])`, `Command::new("bash").args(["-c", user])` — interpreter + user string is RCE even if the binary name is hardcoded.
- `tokio::process::Command` — same pattern.

**Unsafe reachable from user data**
- `unsafe { std::slice::from_raw_parts(ptr, len) }` where `len` comes from user input → OOB read.
- `std::mem::transmute::<_, T>(bytes)` on network bytes → unsoundness across any niche-bearing T (`NonZeroU32`, `bool`, enums).
- `String::from_utf8_unchecked(bytes)` on network bytes → UB in any downstream `str` API.
- `ptr::read` / `ptr::write` with user-derived offsets.

**SQL injection**
- `sqlx::query!` / `sqlx::query_as!` (the MACROS) are compile-time checked and safe.
- `sqlx::query(&format!("... {}", user))` / `.execute(&format!(...))` is SQLi.
- `diesel::sql_query(format!("...{}", user))` is SQLi; use `.bind::<T, _>(val)`.
- `rusqlite::Connection::execute(&format!(...))` is SQLi; use parameterised queries with `params![...]`.

**Deserialization / type confusion**
- `serde_json::from_slice::<Value>` is safe but yields an untyped tree — downstream `value["field"]` walks over user-named keys are the Rust prototype-walk primitive.
- `bincode::deserialize`, `rmp_serde::from_slice`, `ciborium::de::from_reader` without a `#[serde(deny_unknown_fields)]` struct target can parse unbounded trees → DoS or logic confusion.
- `#[serde(flatten)]` on untrusted input merges unknown keys silently; pair with `deny_unknown_fields` at the outer type.

**Path traversal**
- `Path::new(user).starts_with(&base)` checks string prefix, NOT canonicalisation — `base = "/srv"`, `user = "/srv/../etc/passwd"` passes.
- Correct: `let canonical = std::fs::canonicalize(&joined)?; canonical.starts_with(&base_canonical)`.
- `std::fs::read(user_path)`, `File::open(user_path)`, `tokio::fs::*` same.

**SSRF**
- `reqwest::Client::get(user_url)`, `reqwest::get(user_url)` — no default host allowlist; follows redirects.
- `ureq::get(user_url)`, `hyper::Client` direct — same.

**Template / XSS**
- `tera`, `handlebars`, `askama` — autoescape is template-engine-config-dependent. `askama` escapes by extension (`.html` on, `.txt` off). `tera` uses `set_autoescape_suffixes` — check every template.
- Raw-insert helpers: `{{ user | safe }}` in tera, `{{{user}}}` in handlebars.

**Crypto gotchas**
- `rand::random::<u64>()` uses `ThreadRng` which is `CryptoRng` in recent versions — verify the crate version. For tokens, prefer explicit `rand::rngs::OsRng`.
- `==` on `&[u8]` for MAC comparison — use `subtle::ConstantTimeEq` or `ring::constant_time::verify_slices_are_equal`.
- `zeroize` / `secrecy` absence on secret material held across an `.await` or panic — secrets persist in heap.

**Regex**
- `Regex::new(user)` — DoS via pathological backtracking. `regex` crate is linear, but allocation is not bounded — use `RegexBuilder::size_limit` / `dfa_size_limit`.

## Tree-sitter seeds (rust)

```scheme
; Command::new / Command::args
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#eq? @ty "Command")
  (#match? @fn "^(new)$"))

; format! macro — wants cross-check against nearby .execute / .query / .sql_query
(macro_invocation macro: (identifier) @m
  (#match? @m "^(format|write|writeln|println|eprintln)$"))

; .query / .execute / .bind / .sql_query — SQL surface
(call_expression function: (field_expression
    field: (field_identifier) @f)
  (#match? @f "^(query|query_as|execute|sql_query|bind|fetch_one|fetch_all)$"))

; transmute / from_utf8_unchecked / from_raw_parts — unsafe coercion
(call_expression function: (scoped_identifier
    name: (identifier) @fn)
  (#match? @fn "^(transmute|from_utf8_unchecked|from_raw_parts|read|write)$"))
```
