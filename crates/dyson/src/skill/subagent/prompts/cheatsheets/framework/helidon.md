Starting points for Helidon (Java) — not exhaustive. Oracle's MicroProfile-compliant + reactive (Helidon SE) + MicroProfile-style (Helidon MP). Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
**Helidon SE**: `ServerRequest` APIs — `req.queryParams().first("k")`, `req.content().as(String.class)`, `req.content().as(MyType.class)`, `req.headers().first("H")`, `req.path().param("id")`.

**Helidon MP**: JAX-RS annotations (`@QueryParam`, `@PathParam`, etc.) — same as [framework/quarkus.md](quarkus.md).

## Sinks

**SQL (JPA / Helidon DbClient)**
- DbClient: `dbClient.execute().createQuery("SELECT ... " + user)` — SQLi; use named params.
- JPA (Helidon MP): `em.createQuery("... " + user)` — JPQL injection.

**Deserialization (Jackson / JSON-B / Yasson)**
- Helidon defaults to Yasson (JSON-B) in MP mode.  Yasson doesn't have polymorphic-by-default defaults, safer than Jackson.
- Jackson if used: `enableDefaultTyping` → polymorphic RCE.

**Command execution**
- Standard JVM concerns: `Runtime.getRuntime().exec(user)`, `ProcessBuilder`.

**Native image (GraalVM)**
- Helidon supports `mvn package -Pnative-image` — reflection-requiring classes need explicit registration.  Over-broad `reflect-config.json` widens deserialization surface.

**Authentication**
- Helidon SE: `WebServer.builder().addService(Security.builder()...)` — custom Security providers.  A provider returning `AuthenticationResponse.success(subject)` on weak conditions = bypass.
- MP JWT: `mp.jwt.verify.publickey=...` with a leaked private key = forgery.  `mp.jwt.verify.issuer` unset accepts any issuer.
- `@Authenticated` / `@Authorized` annotations — missing on sensitive endpoints = permissive.

**Configuration (`application.yaml`)**
- Plaintext secrets: `security.providers[].config.password=...`.
- `server.host: 0.0.0.0` vs `127.0.0.1` — binding to all interfaces is often intentional but worth flagging when combined with lax auth.
- `tracing` / `openapi` endpoints exposed on the main port — OpenAPI at `/openapi` leaks schema; tracing at `/zipkin` forwards request metadata.

**Reactive pipelines (Helidon SE)**
- `req.content().as(String.class).thenAccept(body -> processUnvalidated(body))` — no schema validation; downstream assumptions can fail.
- Missing `.onError(...)` on a reactive stage — uncaught exceptions may result in stack leaks in responses.

**CORS**
- `CrossOriginConfig.builder().allowOrigins("*").allowCredentials(true).build()` — credentialed wildcard.

**Metrics / health**
- Default endpoints: `/metrics`, `/health`, `/health/live`, `/health/ready`.  If exposed on the main port without auth, reveal operational topology.

## Tree-sitter seeds (java, Helidon-focused)

```scheme
; Helidon Security / MP annotations
(annotation name: (identifier) @a
  (#match? @a "^(Authenticated|Authorized|RolesAllowed|PermitAll|DenyAll|Path|GET|POST|PUT|DELETE|QueryParam|PathParam|HeaderParam|Context|Inject|ApplicationScoped|RequestScoped)$"))

; DbClient / JPA patterns
(method_invocation
  name: (identifier) @m
  (#match? @m "^(createQuery|createNativeQuery|execute|query|namedQuery)$"))
```
