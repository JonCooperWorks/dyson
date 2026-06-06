Starting points for Ktor (Kotlin) — not exhaustive. Kotlin-native server framework; distinct shape from Spring. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`call.parameters["k"]` (path + query), `call.receive<T>()` (JSON/form/binary body), `call.request.headers["H"]`, `call.request.cookies["c"]`, `call.receiveParameters()` (form-urlencoded), `call.receiveMultipart()`.

`call.receive<SomeDataClass>()` uses the installed `ContentNegotiation` feature (typically kotlinx.serialization or Jackson).  Receiving `Map<String, Any>` or `JsonElement` is untyped — the downstream key-walk is the prototype-walk primitive analogue.

## Sinks

**SQL (Exposed ORM / JDBC)**
- Exposed `exec("... ${user}")` — SQLi via string interpolation.  Use `exec("... ?", listOf(user))`.
- `Database.connect(...).transaction { exec("... $user") }` — same.
- Raw JDBC inside Ktor handlers — same concerns as [lang/java.md](../lang/java.md).

**Command execution**
- `Runtime.getRuntime().exec(user)` — RCE; Kotlin inherits Java's process surface.
- `ProcessBuilder(user).start()`.
- `"cmd $user".runtime.exec()` via stdlib extensions.

**File / path**
- `call.respondFile(File(user_path))` — traversal.
- `call.respondOutputStream { ... File(userPath).inputStream().copyTo(this) }`.
- `staticFiles("/static", File(userDir))` at mount time — attacker-derived `userDir` picks serve root.

**Redirect**
- `call.respondRedirect(user_url)` — open redirect.
- `call.respondRedirect(user, permanent = true)` — same.

**XSS / templating**
- `call.respondText(user_html, ContentType.Text.Html)` — raw HTML body, no escape.
- Ktor HTML DSL (`kotlinx.html`) escapes by default.  `unsafe { +user_raw }` explicitly bypasses.
- Freemarker / Pebble / Velocity plugins — same escape-bypass conventions as elsewhere (`?no_esc`, `!user`, etc.).

**Authentication**
- `install(Authentication) { jwt { verifier { ... } validate { ... } } }` — missing `validate` returning a Principal for any token = auth bypass.
- `algorithm = "none"` in JWT config — never correct.
- `authenticate("jwt-auth") { ... }` block needs to wrap every protected route — a route outside the block is unauthenticated.
- `authenticate(optional = true)` in a route scope where auth should be required.

**Sessions**
- `install(Sessions) { cookie<Session>("S") { cookie.extensions["SameSite"] = "lax" } }` — cookies signed via `SignedSessionTransformer`.  A short / hardcoded signing key = session forgery.
- `transform(SessionTransportTransformerEncrypt(encryptKey, signKey))` — both keys required in production; committed literals are a finding.

**CORS**
- `install(CORS) { anyHost(); allowCredentials = true }` — credentialed wildcard CORS.
- `allowHost("example.com", subDomains = listOf("*"))` — be careful; wildcard subdomain of a tenant-multi-domain is overly permissive.

**Deserialization**
- `call.receive<Map<String, Any>>()` — untyped; downstream walk = prototype-walk.
- `call.receive<JsonElement>()` + manual indexing — same.
- kotlinx.serialization with `@Polymorphic` and attacker-controlled discriminator — polymorphic RCE if the module registers dangerous concrete types.

**SSRF**
- `HttpClient().get(user_url)` — no host allowlist.
- Ktor client follows redirects by default; `followRedirects = false` + manual handling is the safe pattern.

## Tree-sitter seeds (kotlin, Ktor-focused)

```scheme
; Route DSL: get("/x") / post("/y") / route { ... }
(call_expression
  (simple_identifier) @m
  (#match? @m "^(get|post|put|delete|patch|options|head|route|authenticate|install)$"))

; call.<sink/source>
(call_expression
  (navigation_expression
    (simple_identifier) @o
    (navigation_suffix (simple_identifier) @m))
  (#eq? @o "call")
  (#match? @m "^(receive|respondText|respondFile|respondRedirect|respondBytes|respondOutputStream|parameters|request)$"))
```
