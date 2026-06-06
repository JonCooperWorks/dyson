Starting points for Spring Framework / Spring Boot (Java + Kotlin) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`@RequestParam`, `@RequestBody`, `@PathVariable`, `@RequestHeader`, `@CookieValue`, `@ModelAttribute`-bound POJOs (mass-assignment risk).

## Sinks

**SQL / JPA injection**
- `@Query("SELECT u FROM User u WHERE u.name = '" + name + "'")` — static JPQL with concat: SQLi.  Use `:name` parameter.
- `entityManager.createQuery("... " + user)` / `createNativeQuery("... " + user)`.
- `JdbcTemplate.query("... " + user, rowMapper)` — use `?` placeholders + `Object[]` args.
- Spring Data JPA `@Query(nativeQuery = true, value = "...")` with SpEL concat: `"SELECT * FROM t WHERE id = #{#user}"` — SpEL injection unless parameterised.

**Mass assignment**
- `public ResponseEntity<User> create(@RequestBody User u)` — every JSON field sets the corresponding entity field.  Attacker sets `admin=true`, `roles=['ROLE_ADMIN']` unless a DTO is used.
- `@ModelAttribute User u` binds form params to entity — same.
- Fix: use separate DTO with only the fields the endpoint should accept; map into entity server-side.

**SpEL injection**
- `@Value("#{systemProperties['user.dir']}")` is fine; `@Value("#{'" + userInput + "'}")` evaluates SpEL on attacker data.
- `expressionParser.parseExpression(user)` in authorization / transformation code — RCE via `T(java.lang.Runtime).getRuntime().exec(...)`.
- `@PreAuthorize("hasRole('" + user + "')")` — SpEL with concat: injection.

**Deserialization (Jackson defaults + polymorphic)**
- `ObjectMapper` with `enableDefaultTyping()` / `activateDefaultTyping()` (pre-2.10) or with `@JsonTypeInfo(use = CLASS)` accepting arbitrary polymorphism on untrusted input → RCE via gadget chains.
- Disable: `objectMapper.deactivateDefaultTyping()`; use explicit `@JsonSubTypes` allowlist.
- XML bindings via JAXB with external entity resolution enabled — XXE.

**Actuator / management endpoints**
- `management.endpoints.web.exposure.include=*` — exposes `/env`, `/heapdump`, `/trace`, `/mappings`.  `/env` on unauth can leak secrets; `/heapdump` leaks everything in memory including session tokens.
- Spring Boot Actuator pre-2.0 had `env` writable → remote config change → RCE via `spring.cloud.bootstrap.location`.
- Flag any actuator exposure without an auth layer.

**Authentication / authorization**
- Missing `@PreAuthorize` / `@Secured` on a controller method handling sensitive ops.
- `permitAll()` on a config that should have `authenticated()`.
- `csrf.disable()` in the security config — CSRF protection off; finding unless the endpoint is API-only with token-based auth.
- `cors().configurationSource(source -> source.setAllowedOrigins("*"))` with credentials = CSRF-equivalent.

**Server-side request forgery**
- `RestTemplate.getForObject(userUrl, ...)` / `WebClient.get().uri(userUrl)` — SSRF; no host allowlist.
- `@RequestMapping("/proxy/**")` handlers that forward the path to an internal URL.

**Path traversal**
- `Resource handler = resourceLoader.getResource("file:" + userPath)` — traversal via `file://` scheme.
- `Paths.get(baseDir, userFilename)` without anchor check.
- `ResponseEntity.ok().body(new FileSystemResource(userPath))` — direct file disclosure.

**Open redirect**
- `"redirect:" + userUrl` as a controller return value — open redirect.
- `ModelAndView("redirect:" + user)`.

**Template injection (Thymeleaf / FreeMarker / Velocity)**
- Thymeleaf `th:utext="${user}"` — raw output (XSS).
- FreeMarker `${user?no_esc}` — escaped by default, bypassed by `?no_esc`.
- `TemplateEngine.process(userTemplate, context)` with `userTemplate` — SSTI → RCE.

## Tree-sitter seeds (java, Spring-focused — Kotlin patterns similar via JVM annotations)

```scheme
; @RequestMapping / @GetMapping / etc. — route entry markers
(annotation
  name: (identifier) @a
  (#match? @a "^(RequestMapping|GetMapping|PostMapping|PutMapping|DeleteMapping|PatchMapping)$"))

; @Query with string concatenation argument
(annotation
  name: (identifier) @a (#eq? @a "Query")) @query

; parseExpression / SpEL entry
(method_invocation
  name: (identifier) @m
  (#match? @m "^(parseExpression|getValue)$"))

; .disable() in a security chain: csrf().disable() / etc.
(method_invocation
  name: (identifier) @m (#eq? @m "disable"))
```
