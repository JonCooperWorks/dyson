Starting points for Dropwizard (Java) — not exhaustive. JAX-RS + Jetty + Jackson + Jersey + Metrics; opinionated production-REST stack. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`@QueryParam("k")`, `@PathParam("id")`, `@HeaderParam("H")`, `@CookieParam("c")`, `@FormParam(...)`, JSON body into method-param POJOs via Jackson.

Dropwizard uses Hibernate Validator (`@NotNull` / `@Size` / `@Valid`).  A POJO without these accepts anything shape-wise.

## Sinks

**SQL (JDBI / Hibernate / raw JDBC)**
- JDBI: `@SqlQuery("SELECT * FROM users WHERE name = '" + :n + "'")` — SQLi.  Use `:n` bind with `@Bind("n") String n` parameter.
- JDBI raw: `handle.execute("..." + user)` — SQLi.
- Hibernate: `session.createQuery("... " + user)` — JPQL injection.
- `Jdbi.open().createQuery("... " + user).mapToBean(...)` — SQLi.

**Deserialization (Jackson)**
- Default Jackson with `enableDefaultTyping()` / `@JsonTypeInfo(use = CLASS)` → polymorphic RCE.
- Dropwizard auto-registers `ObjectMapper` with reasonable defaults; custom `Bootstrap::getObjectMapper().setPolymorphicTypeValidator(...)` / `enableDefaultTyping(...)` is the risky override.
- Jackson YAMLMapper: YAML body into a polymorphic hierarchy — same RCE class.

**Command execution**
- `Runtime.getRuntime().exec(user)` / `ProcessBuilder(user).start()` — RCE.

**File / path**
- `Response.ok(new FileInputStream(userPath))` — traversal.
- `java.nio.file.Files.readAllBytes(Paths.get(userPath))` — same.

**Authentication (Dropwizard auth)**
- `io.dropwizard.auth.basic.BasicCredentialAuthFilter` with an `Authenticator<BasicCredentials, User>` that returns `Optional.of(user)` on any password — bypass.
- `OAuthCredentialAuthFilter` — token validation is authenticator-supplied; a weak validator accepts forged tokens.
- `@PermitAll` / `@RolesAllowed` / `@DenyAll` — JSR-250 annotations.  Missing on sensitive endpoints = permissive by default.
- Chained auth filters: `AuthDynamicFeature(new ChainedAuthFilter<>(List.of(basic, oauth)))` — attacker picks which auth to try; weakest wins.

**Configuration**
- `config.yml` with plaintext DB passwords, OAuth client secrets, encryption keys.  Use env-var substitution (`${DATABASE_PASSWORD}`) not literals.
- `logging.level: DEBUG` on a production config — logs may include request bodies with PII.
- `server.adminConnectors` exposing the admin port (default 8081) — admin endpoints (`/metrics`, `/threads`, `/healthcheck`, `/tasks`) leak internal state and allow `/tasks/gc` etc.  Admin port MUST be network-isolated.

**Tasks (`Task`)**
- `environment.admin().addTask(new Task("trigger-job") {...})` — admin-port tasks that accept query params.  If admin port is exposed, these become attacker-controlled.  `@PostTask` / `GetTask` subclass determines which HTTP method.

**JDBC / connection**
- `dataSourceFactory.getUrl()` with `allowMultiQueries=true` parameter in the JDBC URL — MySQL: attacker SQLi can chain multiple statements.  Default for newer connectors is false; older configs commonly set true.

**Metrics / health endpoints**
- `/healthcheck` on the application port exposes check names (service topology leak).
- `/metrics` on the admin port exposes per-metric values; some contain PII or internal counters.

## Tree-sitter seeds (java, Dropwizard-focused)

```scheme
; JAX-RS resource annotations
(annotation name: (identifier) @a
  (#match? @a "^(Path|GET|POST|PUT|DELETE|PATCH|QueryParam|PathParam|HeaderParam|CookieParam|FormParam|Auth|RolesAllowed|PermitAll|DenyAll|Timed|Metered|ExceptionMetered)$"))

; JDBI annotations
(annotation name: (identifier) @a
  (#match? @a "^(SqlQuery|SqlUpdate|SqlBatch|Bind|BindBean|BindList|RegisterRowMapper)$"))
```
