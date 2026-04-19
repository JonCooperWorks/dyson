Starting points for Java (and Kotlin — JVM shares most sinks) — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `Runtime.getRuntime().exec(user_str)` — RCE.  Array form `exec(new String[]{bin, arg})` is safer but still RCE if `bin` is attacker-controlled.
- `new ProcessBuilder(user_str).start()` — same.
- Kotlin: `"cmd $user".runtime.exec()` via kotlin-stdlib extensions — same.

**Reflection (JVM's prototype-walk primitive)**
- `Class.forName(user_name)`, `ClassLoader.loadClass(user_name)` — class-from-string, then `.newInstance()` / `.getMethod(user).invoke(...)` is the full primitive.
- `Method.invoke(obj, args)` where the method was selected by user-supplied name.
- `Field.get(obj)` / `Field.set(obj, v)` with attacker-selected field name.
- Kotlin `KClass.declaredMembers`, reflection via `::` — same concerns.
- **Property-path reflection walk via JavaBean introspection.**  Any framework that resolves a dotted user-supplied string (`foo.bar.baz`) against a bound object's getter chain is a reflection primitive in disguise.  `PropertyDescriptor`-driven walks will follow `Object.getClass()` into `Class.*`, and once the walk reaches any `Class<?>` method returning a `ClassLoader` / `Module` / `Method` / `Field`, attacker-controlled subsequent segments reach any reachable reflection primitive on the classpath — up to and including `URLClassLoader.newInstance(...)` and arbitrary file writes via server-side pipeline configuration.  Tells: a framework file filters property names through an **explicit BLOCKLIST** (e.g. ignores `classLoader`, `protectionDomain`) — this means the vendor KNOWS the walk reaches those, and is patching known-unsafe names reactively.  Any property name that is NOT on the blocklist but IS reachable and returns a reflection-relevant type IS the finding.  JDK version bumps that add new reachable getters on `Class` / `Object` (JDK9 `getModule()` is the famous one) create blind spots in blocklists written against older JDKs.  File at the filter site; cite the blocklist line; the fix is adding the missing name or switching to an allowlist.

**Deserialization**
- `ObjectInputStream.readObject()` on untrusted bytes — classic Java RCE.  Jackson with default polymorphic typing enabled (`enableDefaultTyping`), XMLDecoder, SnakeYAML with default `Yaml()` (unsafe constructor), Kryo with `RegistrationRequired=false`.
- XStream without a strict allowlist.
- `ObjectMapper#readValue(data, Object.class)` when `@JsonTypeInfo(use = Id.CLASS)` or global default typing is on.
- Kotlinx serialization with `@Polymorphic` + open registration on untrusted input.
- **Reviewing a serialization library *itself* (not an app using one).**  The public read API (`readValue`/`fromXML`/`load`/`parse`/`deserialize`) is the entry, not the sink — every codebase calls it, so it can't be what's CVE-worthy.  The sink is whichever call-site turns a wire-format string (a type-id, a tag, a class-discriminator, a constructor name) into an executable artifact (a `Class<?>`, a `Constructor<?>`, a `Method`).  Concretely: any same-file or sibling-file call that takes a `String` off the parse path and returns a `Class<?>` — `Class.forName`, `ClassLoader.loadClass`, anything named `findClass`/`resolveClass`/`typeFromId`/`classForName`, any `TypeIdResolver` implementation.  Grep strategy: `ast_query` for `method_invocation` with name matching `^(forName|loadClass|findClass|resolveClass|typeFromId)$` across the scope, then `taint_trace max_depth: 32, max_paths: 20` from a wire-read entry to each match.  The chain runs entry → type-resolver → bean-deserializer → reflective setter; the default `max_depth=16` truncates before reaching the setter.  If a validator class exists that checks class names against a list (allow or block), the gap is WHERE the list is incomplete — JDK9+'s `Module` surface is a common blind spot in pre-JDK9 blocklists.

**SQL / JPQL injection**
- `Statement.executeQuery("SELECT ... '" + user + "'")` — string concat in JDBC.  Use `PreparedStatement.setString`.
- JPA `entityManager.createQuery("... " + user)` / `createNativeQuery`.
- Spring `JdbcTemplate.query("... " + user)` without parameter placeholders.
- Hibernate `Query q = session.createQuery("from User where name = '" + user + "'")`.

**Path / file**
- `new File(user)`, `Paths.get(base, user)` — traversal unless canonicalised + prefix-checked.  `user.contains("..")` is not a real check.
- `Files.readAllBytes(Paths.get(user))`.
- Spring `ResourceLoader.getResource("file:" + user)` → `file://` SSRF-equivalent.
- `ZipFile` / `Zip4j` — Zip Slip: untrusted archive entries whose names contain `..` escape the extraction dir if entries are naively `File(dest, entry.getName())`-joined.

**Template / XSS**
- Velocity, FreeMarker, Thymeleaf with user strings concatenated INTO the template (not passed as data): SSTI.
- Thymeleaf `th:utext="${user}"` (raw), JSP `<%= user %>` without `<c:out>` — XSS.
- Kotlin HTML DSL: raw `unsafe { +user }` — XSS.

**XML / XXE**
- `DocumentBuilderFactory.newInstance()` — XXE by default pre-17; disable external entities: `setFeature("http://apache.org/xml/features/disallow-doctype-decl", true)`.
- `SAXParser` / `TransformerFactory` / `SchemaFactory` — same secure defaults must be set.

**Crypto**
- `Cipher.getInstance("AES")` — defaults to ECB mode.  Specify `AES/GCM/NoPadding`.
- `MessageDigest.getInstance("MD5"|"SHA-1")` for password hashing / auth tokens.
- `new Random()` for session IDs / tokens — use `SecureRandom`.
- `String.equals` on HMACs / MACs — timing-unsafe; use `MessageDigest.isEqual`.

## Scope-delegation dismissal — NOT a mitigation

Applies to every sink class above — deserialization, reflection, SQL, template, XXE, command exec.

When an in-scope class receives attacker-controlled input and then calls an unsafe operation that physically lives in a sibling package, parent jar, or the JDK (`com.fasterxml.jackson.core.*`, `org.yaml.snakeyaml.constructor.*`, `java.lang.reflect.*`, anything outside the review root), **the in-scope class is still the finding**.  The wrapper is the attacker's API — the method an attacker reaches over the wire.  The sink being one `import` away does not exonerate the wrapper.

Phrases to reject verbatim:
- "the reflective invoke lives in another jar / sibling package — out of scope"
- "delegates to X in `com.fasterxml.jackson.core.*` — not reviewed here"
- "the actual deserialization call is in the JDK / core library"
- "this class just configures, the unsafe op is elsewhere"

How to file it:
1. **File at the in-scope class's public method or constructor** — the entry point attacker input reaches first.
2. **Cite the delegation call site as the sink line** (the `Class.forName(typeId)` call inside your `TypeIdResolver`, the `Method.invoke(...)` call inside your `BeanDeserializer`, the `Yaml.load(stream)` call inside your public loader wrapper).
3. **In Impact, describe the downstream unsafe op** ("reflective class instantiation via `com.fasterxml.jackson.databind.util.ClassUtil.createInstance`") so the reader sees the full chain.
4. **Do not move the wrapper to `Checked and Cleared`** with an "outside scope" reason.  Wrapping is the defense you own and there isn't one.

## Tree-sitter seeds (java)

```scheme
; Runtime.getRuntime().exec(...)
(method_invocation
  name: (identifier) @m
  (#eq? @m "exec"))

; Class.forName / loadClass — reflection entry
(method_invocation
  name: (identifier) @m
  (#match? @m "^(forName|loadClass|getMethod|getDeclaredMethod|invoke)$"))

; Statement.executeQuery / PreparedStatement creation
(method_invocation
  name: (identifier) @m
  (#match? @m "^(executeQuery|executeUpdate|execute|createQuery|createNativeQuery)$"))

; ObjectInputStream.readObject
(method_invocation
  name: (identifier) @m
  (#eq? @m "readObject"))
```
