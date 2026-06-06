Starting points for Quarkus (Java / Kotlin) — not exhaustive. Native-mode ahead-of-time compilation + JAX-RS / Reactive Routes. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`@QueryParam("k") String k`, `@PathParam("id") String id`, `@HeaderParam("H")`, `@CookieParam("c")`, `@FormParam(...)`, `@BeanParam MyBean`, JSON body into `@RequestScoped` / method-param POJOs.

## Sinks

**SQL (Hibernate / Panache / native SQL)**
- JPQL: `em.createQuery("SELECT u FROM User u WHERE u.name = '" + name + "'")` — interpolation: JPQL injection.  Use `:name` parameter.
- Panache: `Person.find("name = '" + user + "'")` — SQLi.  `find("name = ?1", user)` is parameterised.
- `em.createNativeQuery("SELECT ... " + user)` — native SQLi.

**Reactive Routes (`@Route`)**
- Handlers receive `RoutingContext`.  `ctx.request().getParam("k")` — attacker-controlled.  Same Vert.x-style flow (Quarkus reactive uses Vert.x underneath).
- `ctx.response().putHeader("Location", userUrl).setStatusCode(302).end()` — open redirect.

**Deserialization (Jackson + JSON-B)**
- Jackson in Quarkus auto-registers modules at startup.  `@JsonTypeInfo(use = Id.CLASS)` + `ObjectMapper` accepting any polymorphic class → RCE via gadget chains.
- Quarkus: `@RegisterForReflection` exposes classes to native-image reflection; combined with user-chosen type discriminators = amplified polymorphic deserialization risk.

**Native image pitfalls**
- GraalVM native-image mode strips unused classes.  A reflection-heavy dependency requires `resources-config.json` / `reflect-config.json` / `@RegisterForReflection`.  Loose `@RegisterForReflection` annotations on DTOs widen the deserialization surface.
- `-H:+AddAllCharsets` / `-H:+IncludeAllLocales` flags — not a finding, but worth knowing the native build's runtime surface differs from JVM.

**Config-level**
- `application.properties` / `application.yaml` with committed secrets: `quarkus.datasource.password=...`, `quarkus.oidc.credentials.secret=...`.
- `quarkus.dev.ui` exposed in production profile — web dev UI with reflection and entity inspection.
- `quarkus.http.cors.origins=*` + `quarkus.http.cors.access-control-allow-credentials=true` — credentialed wildcard.
- `quarkus.security.users.embedded` with hardcoded users/passwords in properties.

**MicroProfile JWT**
- `mp.jwt.verify.issuer=https://trusted-idp/` must be set; empty / missing = any issuer accepted.
- `mp.jwt.verify.publickey=...` — committed public key is fine; committed PRIVATE key is a finding.
- `mp.jwt.token.header=Authorization` default.  Custom header names (`X-Jwt`) trusted as identity without per-request verification middleware = bypass.

**Panache secrets / `LaunchMode.DEVELOPMENT`**
- `LaunchMode.current() == LaunchMode.DEVELOPMENT` code paths that log full entity contents or bypass auth — must not reach production binaries.  Look for `if (Launch...DEVELOPMENT)` checks and confirm build profile separation.

**CDI / `@RequestScoped` side channels**
- `@SessionScoped` bean holding cross-request data without tenant scoping — data leaks between requests.
- `@Inject HttpServerRequest req` at class scope in an `@ApplicationScoped` bean — class-level caching of per-request state = stale auth.

## Tree-sitter seeds (java, Quarkus-focused)

```scheme
; JAX-RS annotations + Quarkus-specific
(annotation name: (identifier) @a
  (#match? @a "^(Path|GET|POST|PUT|DELETE|PATCH|QueryParam|PathParam|HeaderParam|CookieParam|FormParam|BeanParam|RolesAllowed|PermitAll|DenyAll|RegisterForReflection)$"))

; em.createQuery / createNativeQuery
(method_invocation name: (identifier) @m
  (#match? @m "^(createQuery|createNativeQuery|createNamedQuery|find|findAll)$"))
```
