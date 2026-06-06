Starting points for Javalin (Java / Kotlin) — not exhaustive. Minimal micro-framework; security is almost entirely on the handler. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`ctx.queryParam("k")`, `ctx.formParam("k")`, `ctx.pathParam("id")`, `ctx.header("H")`, `ctx.cookie("c")`, `ctx.bodyAsClass(MyDto::class.java)`, `ctx.bodyValidator(MyDto::class.java)`, `ctx.uploadedFile("f")`, `ctx.body()` (raw string).

`ctx.bodyValidator<T>()` runs Javalin's validator chain — without `.check { ... }` predicates it only asserts the body is parseable into `T`.  Not a shape validator by itself.

## Sinks

**SQL (JDBC / Exposed / jOOQ)**
- `stmt.executeQuery("SELECT ... '" + ctx.pathParam("id") + "'")` — SQLi; use `PreparedStatement`.
- Exposed (Kotlin): `.where { MyTable.name eq ctx.queryParam("name") }` is parameterised.  `exec("... ${ctx.queryParam("name")}")` is SQLi.
- jOOQ `create.fetch("... " + user)` — raw SQL; use `param("n", user)`.

**Command execution**
- `Runtime.getRuntime().exec(ctx.queryParam("cmd"))` — RCE.
- Kotlin: `"bash -c '$cmd'".runtime.exec()` — shell RCE.

**File / path**
- `ctx.result(FileInputStream(userPath))` / `ctx.resultFile(File(userPath))` — traversal.
- `app.config.staticFiles.add(userRoot)` at config time — attacker-derived serve root if config is user-writable.
- Javalin's `StaticFileConfig.precompress` doesn't protect against traversal; canonicalise + anchor.

**Redirect**
- `ctx.redirect(userUrl)` — open redirect.
- `ctx.redirect(userUrl, HttpStatus.FOUND)` — same.

**XSS**
- `ctx.html(userHtml)` — raw HTML body.
- `ctx.result(userHtml)` with explicit `ctx.contentType("text/html")`.
- No built-in template engine; integrations (Velocity, Freemarker, Pebble, JTE, Mustache) carry their own escape conventions — `| safe`, `{{{ }}}`, `!{...}` etc. bypass.

**Auth / access manager**
- `app.beforeMatched { ctx -> ... auth ... }` running globally — a route registered as unauthenticated (`RouteRole.ANYONE` vs `RouteRole.AUTHENTICATED`) skips the auth filter.
- Custom `AccessManager` that always `handler.handle(ctx)` — auth effectively disabled.
- `javalin-jwt` with hardcoded HMAC secret.

**Deserialization**
- `ctx.bodyAsClass(Map::class.java)` — untyped; downstream `map[user_key]` walks = prototype-walk analogue.
- Default Jackson integration — `enableDefaultTyping()` / `@JsonTypeInfo(use = CLASS)` is polymorphic-RCE-adjacent; check if enabled.

**WebSocket**
- `ws.onMessage { ctx -> ... ctx.message() ... }` — attacker-controlled frames.  Feeding into `Jackson.readValue(..., Map::class.java)` + walking keys = prototype-walk.
- `wsBefore { ctx -> authenticateSession(ctx) }` missing — anonymous WebSocket connections.

**CORS (`CorsPlugin`)**
- `cfg.plugins.enableCors { it.add { anyHost(); allowCredentials = true } }` — credentialed wildcard CORS.

**Error leakage**
- `app.exception(Exception::class.java) { e, ctx -> ctx.result(e.stackTraceToString()) }` — stacktrace leak.
- Default dev mode logs full request bodies; production mode should not.

## Tree-sitter seeds (java / kotlin, Javalin-focused)

```scheme
; Route DSL: app.get / .post / .ws / etc.
(method_invocation name: (identifier) @m
  (#match? @m "^(get|post|put|delete|patch|options|head|ws|before|after|beforeMatched|exception|error|routes)$"))

; ctx.<source / sink>
(method_invocation name: (identifier) @m
  (#match? @m "^(queryParam|formParam|pathParam|header|cookie|bodyAsClass|bodyValidator|body|uploadedFile|redirect|result|html|json|contentType|status)$"))
```
