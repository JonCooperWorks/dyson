Starting points for Micronaut (Java / Kotlin / Groovy) — not exhaustive. AOT-compiled like Quarkus; compile-time DI instead of reflection. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`@QueryValue("k")`, `@PathVariable("id")`, `@Header("H")`, `@CookieValue("c")`, `@Body MyDto`, `@Part` (multipart), `HttpRequest<T>` for raw access.

Micronaut Data / Micronaut Serialization are compile-time safe versus reflection-based alternatives; but raw SQL and `@QueryHint("...")` hatches still exist.

## Sinks

**SQL (Micronaut Data / JDBC)**
- `@Repository interface UserRepo { @Query("SELECT u FROM User u WHERE u.name = '" + :n + "'") }` — at annotation time, the string is compile-time, but interpolation-at-build via code generation is rare.  The real risk: a repo method taking a `String` and building dynamic JPQL in a `@QueryHint` or via a custom `CriteriaBuilder` with user-supplied column names.
- `jdbcOperations.execute("... " + user)` — native SQLi.
- Hibernate via Micronaut: same raw-query risks as [framework/spring.md](spring.md) / [framework/quarkus.md](quarkus.md).

**Deserialization**
- Micronaut Serialization (compile-time) does NOT have the polymorphic-by-default pitfall of Jackson — bean types must be explicitly opted in via `@Serdeable`.
- If the app uses Jackson directly (`@MicronautBootstrap` importing `jackson-databind`), polymorphic typing with `enableDefaultTyping()` is RCE.  Check for the Jackson dep alongside Micronaut.

**Command execution**
- `Runtime.getRuntime().exec(user)` — JVM-level, applies here.
- `ProcessBuilder(user).start()`.

**Reactive types & thread safety**
- `Mono<T>` / `Flux<T>` handlers: the reactive chain carries per-subscriber context.  A `@Controller` field holding per-request state (not in `Mono.deferContextual`) — cross-request leak.
- Blocking I/O on reactive event loops (`runBlocking { dbQuery() }` on Netty's IO threads) — performance issue; not security unless it masks auth timeouts.

**Security (`micronaut-security-jwt` / `-session` / `-oauth2`)**
- `@Secured(SecurityRule.IS_ANONYMOUS)` on an endpoint that handles sensitive data — finding.
- Missing `@Secured` on a controller method falls back to the controller-level `@Secured`; absent at both levels = `SecurityRule.IS_AUTHENTICATED` by default in some configs, `IS_ANONYMOUS` in others.  The default depends on `micronaut.security.intercept-url-map` — check config.
- `security.token.jwt.signatures.secret.generator.secret = 'dev'` — hardcoded HMAC key.
- `security.token.jwt.signatures.jwks.*` without audience / issuer pinning — accepts JWTs from any JWKS endpoint reachable.
- `security.filter.paths = ['/**']` vs specific patterns: check the filter covers every protected route.

**File / path**
- `@Controller("/files")` returning `StreamedFile(userPath)` — traversal.
- `File(userRoot)` via `@Value("${storage.root}")` with config ever user-writable — attacker-derived serve root.

**Redirect / SSRF**
- `HttpResponse.redirect(URI.create(userUrl))` — open redirect.
- Micronaut HTTP Client with user-supplied URL → SSRF.

**GraalVM native-image**
- Same surface concerns as [framework/quarkus.md](quarkus.md).  Reflection-requiring deps need `reflect-config.json`; broad reflection configs widen the deserialization surface.

**`@Filter`s**
- Order matters; a `@Filter("/**")` with `@Order(HIGHER_PRECEDENCE)` running before an auth filter can read sensitive body content.
- `@ServerFilter` in Micronaut 4 replaces `@Filter`; same concerns.

## Tree-sitter seeds (java, Micronaut-focused)

```scheme
; Micronaut route / param annotations
(annotation name: (identifier) @a
  (#match? @a "^(Controller|Get|Post|Put|Delete|Patch|QueryValue|PathVariable|Header|CookieValue|Body|Part|Secured|Filter|ServerFilter|Repository|Query|QueryHint|Value)$"))

; jdbcOperations / jdbcConnection.execute / em.createQuery
(method_invocation name: (identifier) @m
  (#match? @m "^(execute|executeUpdate|executeQuery|createQuery|createNativeQuery)$"))
```
