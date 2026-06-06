Starting points for Eclipse Vert.x (Java / Kotlin) — not exhaustive. Event-driven, non-blocking; each verticle runs on an event loop. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`ctx.request().getParam("k")`, `ctx.queryParam("k")`, `ctx.body().asJsonObject()`, `ctx.body().asString()`, `ctx.pathParam("id")`, `ctx.request().getHeader("H")`, `ctx.request().cookies()`, `ctx.fileUploads()`.

## Sinks

**SQL (vertx-jdbc-client / vertx-pg-client / vertx-mysql-client)**
- Vert.x SQL clients expect `String.format` / `?` placeholders.  `connection.preparedQuery("... ?").execute(Tuple.of(user))` is parameterised.  `connection.query("... " + user).execute()` is SQLi.
- `connection.preparedQuery("... #{user}").execute()` — `#{}` is NOT Vert.x placeholder syntax; evaluates as a literal `#{user}` embedded in SQL.  Developers coming from JPA misuse this.

**Event bus (Vert.x's core IPC)**
- `vertx.eventBus().send("address", userMessage)` — the event bus is a string-addressed pub/sub.  If an address is user-derivable, attacker picks which consumer receives the message.
- `vertx.eventBus().consumer("address", msg -> { ... })` — msg body is attacker-controlled (from whoever can send to the address).
- Distributed event bus over the network (clustered mode): cluster members trust each other; an unauthenticated cluster join from a rogue node injects messages.

**Verticle deployment**
- `vertx.deployVerticle(userName, options)` — `userName` as a class name / module name = arbitrary class loading → RCE.
- `vertx.deployVerticle("js:./userScript.js", options)` (with polyglot engine) — attacker script = RCE.

**Command execution**
- `Runtime.getRuntime().exec(user)` — blocks the event loop PLUS RCE.
- Kotlin: same.

**File / path (async FS)**
- `vertx.fileSystem().readFile(userPath, handler)` — traversal.
- `ctx.response().sendFile(userPath)` — traversal.
- Static handler: `StaticHandler.create().setWebRoot(userRoot)` — attacker-derived serve root.

**Redirect**
- `ctx.response().putHeader("Location", userUrl).setStatusCode(302).end()` — open redirect.
- `ctx.redirect(userUrl)` — same.

**Deserialization**
- `ctx.body().asPojo(MyClass.class)` via Jackson (default ObjectMapper) — polymorphic-typing concerns same as [framework/spring.md](spring.md).
- `ctx.body().asJsonObject()` returns `JsonObject` (untyped tree); downstream `jsonObject.getString(user_key)` walks over attacker keys.

**Authentication**
- `AuthenticationHandler.create(authProvider)` — authProvider implementation is developer-supplied.  Weak implementations return `Future.succeededFuture(new UserImpl(...))` on any token.
- `JWTAuth.create(vertx, new JWTAuthOptions().addPubSecKey(new PubSecKeyOptions().setAlgorithm("HS256").setBuffer("dev")))` — hardcoded key.
- Route-level: `router.route("/admin/*").handler(authHandler)` — routes registered directly on `router` (not under `/admin/*`) bypass.

**CORS**
- `CorsHandler.create("*").allowCredentials(true)` — credentialed wildcard.

**Body size / DoS (out of scope per rules)**
- `router.route().handler(BodyHandler.create().setBodyLimit(-1))` — no body limit.  Worth flagging if a handler performs significant work per request (memory exhaustion).

**Event-loop blocking**
- Blocking operations (JDBC, `File.readAllBytes`, `Thread.sleep`) inside a handler stall the event loop.  Not a direct security finding; can mask timing-based auth and lead to starvation under load.

## Tree-sitter seeds (java, Vert.x-focused)

```scheme
; router.route / .get / .post / .handler / .failureHandler
(method_invocation
  name: (identifier) @m
  (#match? @m "^(route|get|post|put|delete|patch|options|head|handler|failureHandler|blockingHandler|consumer|send|publish)$"))

; ctx.<source / sink>
(method_invocation
  name: (identifier) @m
  (#match? @m "^(getParam|queryParam|body|pathParam|getHeader|cookies|fileUploads|redirect|response|request|end)$"))
```
