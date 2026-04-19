Starting points for Vapor (Swift server) — not exhaustive. Type-safe routing + async-await; shape-validation via Codable closes many classes. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`req.query["k"]` (typed via `req.query.get(String.self, at: "k")` or decoded into `struct`), `req.content.decode(MyDTO.self)`, `req.parameters.get("id")`, `req.headers["H"]`, `req.cookies["c"]`, `req.body.collect()` (raw bytes), multipart uploads.

`req.content.decode(T.self)` uses Swift `Codable`; decoding into a concrete struct gives you shape validation for free.  Decoding into `[String: String]` or `[String: AnyCodable]` is untyped — downstream `dict[user_key]` access is the prototype-walk primitive analogue.

## Sinks

**SQL (Fluent ORM + raw SQL)**
- Fluent typed queries are safe.  Raw `database.raw("... \(user)")` — interpolation is concat; use `.bind(user)`.
- `database.raw(.init(stringLiteral: "SELECT \(user)"))` — interpolation bypass.
- Postgres driver: `conn.query("... \(user)")` — same.

**Command execution**
- `Process()` + `launchPath = user` — RCE.
- `try Process.run(URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", user])` — shell RCE.

**File / path**
- `req.fileio.streamFile(at: userPath)` — traversal unless anchored.
- `app.middleware.use(FileMiddleware(publicDirectory: userDir))` at configure time — `userDir` from config ever user-writable picks serve root.
- `req.application.directory.publicDirectory + userName` — attacker filename; use `URL(fileURLWithPath:)` with `.lastPathComponent` (basename) + realpath prefix check.

**Redirect**
- `req.redirect(to: userUrl)` — open redirect.
- `Response(status: .found, headers: ["location": userUrl])` — same.

**XSS / templates**
- Leaf: `#(user)` escapes by default.  `#unsafeHTML(user)` bypasses.
- `Response(status: .ok, headers: ["content-type": "text/html"], body: .init(string: userHTML))` — raw HTML body.

**Deserialization / polymorphism**
- `try req.content.decode(MyCodable.self)` with `MyCodable` using a custom `init(from:)` that switches on an attacker-provided discriminator to instantiate different concrete types — polymorphic parsing.  Check the `init(from:)` body.
- `JSONDecoder().decode([String: AnyCodable].self, from: bytes)` + downstream walk = prototype-walk-equivalent.

**SSRF**
- `req.client.get(URI(string: userUrl))` — SSRF via Vapor's client.  No default host allowlist.
- `HTTPClient().get(url: userUrl)` — same.

**Authentication**
- `app.middleware.use(UserAuthenticator())` — auth middleware registered at app scope.  A route group NOT wrapped in `.grouped(UserAuthenticator())` is unauthenticated.
- `req.auth.require(User.self)` missing on a handler that should require auth.
- `User.authenticator()` where the authenticator's `authenticate` method returns `req.auth.login(user)` on any token — bypass.
- JWT: `app.jwt.signers.use(.hs256(key: "dev"))` — hardcoded key.

**CORS**
- `CORSMiddleware.Configuration(allowedOrigin: .all, allowedMethods: [.GET, .POST, .PUT, .OPTIONS, .DELETE, .PATCH], allowedHeaders: [], allowCredentials: true)` — credentialed wildcard CORS.

**Sessions**
- `app.sessions.use(.memory)` (default) — sessions in-memory, fine for dev.  `.redis(...)` in production; ensure the Redis connection string isn't a committed literal with credentials.
- `app.middleware.use(app.sessions.middleware)` — session middleware must be registered before auth middleware that reads session state.

**Crypto**
- `Crypto.SHA256.hash(data:)` / `Crypto.MD5.hash(data:)` — CryptoKit's `Insecure.MD5` and `Insecure.SHA1` are prefixed "Insecure"; don't use for password hashing (use BCrypt / Argon2 via a third-party lib).
- `Bcrypt.hash("password")` with cost < 10 — weak cost parameter.

## Tree-sitter seeds (swift, Vapor-focused)

```scheme
; Route DSL: app.get("/x") / app.post / etc.
(call_expression
  (member_access_expression
    name: (identifier) @m)
  (#match? @m "^(get|post|put|delete|patch|on|group|grouped)$"))

; req.<source / sink>
(call_expression
  (member_access_expression
    name: (identifier) @m)
  (#match? @m "^(query|content|parameters|headers|cookies|redirect|view|render|fileio)$"))
```
