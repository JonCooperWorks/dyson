Starting points for Kotlin — not exhaustive. JVM underneath, so [lang/java.md](java.md) applies verbatim for reflection / deserialization / crypto / SQL — this sheet adds Kotlin-specific shapes. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `Runtime.getRuntime().exec(user)` (from Java) — same risk.
- Kotlin shortcut: `"bash -c '$user'".runtime.exec()` via common extensions — RCE.
- `ProcessBuilder(user)` — same as Java.

**Dynamic code / reflection**
- `::class.java.getMethod(user).invoke(obj, args)` — JVM reflection via Kotlin's class reference syntax.
- `Class.forName(user).kotlin.createInstance()` — arbitrary class instantiation.
- `KClass<*>::memberFunctions.first { it.name == user }.call(args)` — Kotlin reflection via `kotlin-reflect`.
- Scripting: `ScriptEngineManager().getEngineByExtension("kts").eval(user_str)` — Kotlin SSTI / RCE.

**Deserialization**
- `kotlinx.serialization.Json { serializersModule = ... }` with `@Polymorphic` over a permissive `SerializersModule` — polymorphic RCE if the module registers dangerous concrete types.
- `ObjectInputStream.readObject()` — inherited Java concern; flag if reachable from a Kotlin handler.
- Jackson-kotlin-module defaults are saner than plain Jackson, but `enableDefaultTyping` / `activateDefaultTyping` still apply.

**Null safety is not security**
- `user!!` force-unwrap on a nullable path — NPE → 500 → DoS (out of scope per rules).  But can mask an auth check: `user!!.id` on a null user panics instead of rejecting.
- Elvis with unsafe default: `params["role"] ?: "admin"` — attacker-absent key defaults to admin.

**Coroutine scope leaks**
- `GlobalScope.launch { ... sensitive action ... }` — ignores structured concurrency; sensitive work may outlive the request context, run after auth context is gone.  Not a finding alone, but combined with request-scoped auth state it's a TOCTOU-style issue.

**Ktor-specific (most common Kotlin server framework)**
- See [framework/spring.md](../framework/spring.md) for Spring-on-Kotlin.  Ktor has:
  - `call.receive<Map<String, Any>>()` — untyped body, attacker JSON; downstream key-walk is the prototype-walk primitive equivalent.
  - `call.respondRedirect(user_url)` — open redirect.
  - `call.respondFile(File(user_path))` — traversal.
  - `install(CORS) { anyHost() }` — permissive CORS.
  - Routes without an `authenticate { ... }` wrapper — unauthenticated access.

**SQL / Exposed ORM**
- Exposed `exec("... ${user}")` — SQLi via interpolation.  Use `exec("... ?", listOf(user))`.
- `Query.andWhere { stringLiteral(user) }` — raw fragments without parameterisation.

**Crypto**
- Inherits Java crypto surface: `MessageDigest.getInstance("MD5")`, ECB, `String.equals` on MAC.
- `kotlinx.crypto` is fine; stdlib `java.security.SecureRandom` is fine.

## Tree-sitter seeds (kotlin)

```scheme
; Method call / reflection entry
(call_expression
  (navigation_expression
    (navigation_suffix (simple_identifier) @m))
  (#match? @m "^(exec|forName|createInstance|invoke|call|eval)$"))

; receive<T>() / respond*() — Ktor
(call_expression
  (navigation_expression
    (navigation_suffix (simple_identifier) @m))
  (#match? @m "^(receive|respondRedirect|respondFile|respondText|respond)$"))
```

Kotlin's tree-sitter grammar (`tree-sitter-kotlin-ng`) is relatively new — always `ast_describe` a representative snippet before writing a non-trivial query.
