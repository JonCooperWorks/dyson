Starting points for Play Framework (Java / Scala) — not exhaustive. Reactive MVC; runs on top of Akka. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`request().queryString().get("k")`, `request().body().asJson()`, `request().body().asFormUrlEncoded()`, `request().body().asRaw()`, `request().headers().get("H")`, `request().cookies().get("c")`, route params as method arguments.

Form binding via `Form<T>` validates against a bound class.  Without `.validate()` or JSR-303 annotations on the class, shape enforcement is minimal.

## Sinks

**SQL (Slick / Anorm / JPA / Ebean)**
- Anorm: `SQL("SELECT ... '" + user + "'")` — SQLi.  Use `SQL("... {p}").on('p -> user)`.
- Slick: `sql"SELECT ... #$user"` — `#$` interpolates AS LITERAL SQL (no escape).  `$user` is parameterised via Slick's safe-interpolation.  Confusing; check each use.
- Ebean: `Ebean.createQuery("SELECT u FROM User u WHERE u.name = '" + user + "'")` — JPQL injection.

**Deserialization**
- Play's `Json.fromJson[MyCaseClass](body)` (Scala) / `Json.fromJson(body, MyClass.class)` (Java) — safe IF target type is sealed.  Into `JsValue` + walking fields = prototype-walk analogue.
- Java: Jackson config — polymorphic `enableDefaultTyping` → RCE.

**Command execution**
- `Runtime.getRuntime().exec(user)` / `ProcessBuilder(user).start()` — JVM-wide, RCE.
- Scala `sys.process.Process(user).!` — shell execution.

**Template (Twirl)**
- `@Html(user)` — bypasses auto-escape.  Default `@user` is HTML-escaped.
- `@{user}` — evaluates Scala expression; if user string is passed directly, it's the `toString` value, auto-escaped.  Combined with manual string construction → XSS.

**Redirect**
- `Redirect(user_url)` / `Redirect(url = user_url, status = 302)` — open redirect.
- `TemporaryRedirect(user_url)` — same.

**File / path**
- `Ok.sendFile(new File(user_path))` — traversal.
- `Ok.sendPath(Paths.get(user_path))` — same.
- Play's `Assets.versioned("/public", user_path)` — anchor is enforced by Assets; custom file-serving endpoints need manual anchoring.

**CSRF**
- Play's CSRF filter is global but `@nocsrf` annotation on a route disables it.  Check routes config for explicit CSRF exclusions on state-changing routes.
- API endpoints (`/api/*`) often bypass CSRF by convention; if they accept session cookies, CSRF is still relevant.

**Authentication**
- Custom `Security.AuthenticatedAction` extending `Action` — the `authenticate` logic is developer-implemented.  Returning a user based on a raw cookie without signing-check = forged identity.
- Deadbolt / Silhouette integrations — check the authenticator strategy.
- `request().session().get("user_id")` trusted without re-fetching from DB each time → stale roles.

**Configuration (`application.conf`)**
- `play.http.secret.key = "dev"` — hardcoded signing key.  Compromises session signing + CSRF token signing.
- `play.filters.hosts.allowed = ["."]` — permissive allowed-hosts; enables Host-header attacks.
- `play.filters.cors.allowedOrigins = ["*"]` + credentialed requests — wildcard CORS.
- `play.evolutions.autoApply = true` in production — runs migrations automatically; combined with a hacked DB URL = attacker-controlled schema changes.

**WebSocket (`WebSocket`)**
- `WebSocket.accept[String, String]` — each message is attacker-controlled.  Feeding into JSON-parse + field walk is prototype-walk analogue.
- No auth by default — check connection-time authentication in the `acceptOrResult` callback.

## Tree-sitter seeds (java, Play-focused — Scala patterns similar)

```scheme
; Routes annotation (Java): @Security.Authenticated
(annotation name: (identifier) @a
  (#match? @a "^(Security|AddCSRFToken|RequireCSRFCheck|NoCSRFCheck|Cached|With|Authenticated)$"))

; SQL / template
(method_invocation
  name: (identifier) @m
  (#match? @m "^(createQuery|createNativeQuery|sendFile|sendPath|asJson|asFormUrlEncoded|asRaw)$"))

; Redirect / Ok.body chains
(method_invocation
  name: (identifier) @m
  (#match? @m "^(Redirect|TemporaryRedirect|PermanentRedirect|ok|badRequest|notFound)$"))
```
