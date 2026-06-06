Starting points for Hummingbird (Swift server) — not exhaustive. Async-await native alternative to Vapor; thinner framework, closer to bare swift-nio. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request.uri.queryParameters["k"]`, `request.parameters["id"]` (path), `try await request.decode(as: MyDTO.self, context: context)`, `request.headers["H"]`, `request.cookies["c"]`, `try await request.body.collect(upTo: N)`.

Hummingbird's `decode` uses `Codable` + the configured decoder (JSON by default).  Typed DTOs give shape-validation.  `HBRequestDecoder` into `[String: Any]` via `AnyCodable` = untyped.

## Sinks

**SQL (via PostgresNIO / MySQLNIO / etc.)**
- `conn.query("... \(user)")` using string-interpolation — SQLi.  Use placeholder binding.
- Raw Postgres `conn.simpleQuery("... \(user)")` — no parameterisation possible; never interpolate user data.

**Command execution**
- `Process()` + `launchPath = user` — RCE; same as [framework/vapor.md](vapor.md).
- `try Process.run(URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", user])`.

**File / path**
- `try await request.body.collect(upTo: N)` decoded into a filename + `URL(fileURLWithPath: userName).lastPathComponent` + directory join — traversal unless anchored.
- `HBFileMiddleware(publicDirectory: userDir)` — attacker-derived serve root at middleware construction.

**Redirect**
- `response.headers["Location"] = userUrl; response.status = .found` — open redirect.
- Return `HBResponse(status: .found, headers: ["location": userUrl])`.

**XSS**
- `HBResponse(status: .ok, headers: ["content-type": "text/html"], body: .byteBuffer(ByteBuffer(string: userHTML)))` — raw HTML body.
- Mustache / Leaf integrations — each has its own escape-bypass idiom.

**Auth middleware**
- `router.group()` + `.add(middleware: HBAuthenticatorMiddleware())` — auth on a group.  Routes outside the group are unauthenticated.
- Custom authenticator returning `Principal` for any request — auth bypass.
- `HBJWTAuthenticator` configured with a hardcoded secret or weak algorithm.

**Deserialization / polymorphism**
- `try await request.decode(as: JSONValue.self)` with a custom JSONValue enum walking keys — prototype-walk analogue.
- Custom `init(from:)` on DTOs that instantiates different concrete types based on a discriminator string — polymorphic-RCE-adjacent.

**SSRF**
- `HBClient().get(url: userUrl)` — no default allowlist.
- URLSession via swift-nio: attacker URL reachable from the server's egress.

**Concurrency / actor boundaries**
- Global actor references (`@MainActor`) used for request-scoped state — cross-request contamination if not scoped to `@Sendable` closures per-request.

**TLS**
- `HBApplication(configuration: .init(tlsOptions: nil))` — cleartext HTTP.  For production, `TSTLSOptions` must be set or a reverse proxy must terminate TLS.

## Tree-sitter seeds (swift, Hummingbird-focused)

```scheme
; Route DSL: router.get / .post / .group / .add
(call_expression
  (member_access_expression
    name: (identifier) @m)
  (#match? @m "^(get|post|put|delete|patch|options|group|add|on|head|use)$"))

; request.decode / request.uri / request.body
(call_expression
  (member_access_expression
    name: (identifier) @m)
  (#match? @m "^(decode|query|parameters|headers|cookies|body|collect)$"))
```
