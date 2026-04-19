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

**Deserialization**
- `ObjectInputStream.readObject()` on untrusted bytes — classic Java RCE.  Jackson with default polymorphic typing enabled (`enableDefaultTyping`), XMLDecoder, SnakeYAML with default `Yaml()` (unsafe constructor), Kryo with `RegistrationRequired=false`.
- XStream without a strict allowlist.
- `ObjectMapper#readValue(data, Object.class)` when `@JsonTypeInfo(use = Id.CLASS)` or global default typing is on.
- Kotlinx serialization with `@Polymorphic` + open registration on untrusted input.
- **Polymorphic-deser gadget-chain wrappers.**  Jackson/XStream/SnakeYAML route from the public entry (`readValue`, `fromXML`, `Yaml.load`) through a type-resolver → bean-deserializer → reflective setter chain.  When the in-scope file is the wrapper (`BeanDeserializer`, `AsPropertyTypeDeserializer`, `SubTypeValidator`, a `TypeIdResolver` impl) and the reflective-invoke lands in a sibling package, that is STILL the finding — file at the wrapper.  The chain depth is why `taint_trace` for this class usually needs `max_depth: 32, max_paths: 20`; the default cap (16/10) truncates before reaching the setter sink.

**Scope-delegation dismissal is NOT a mitigation.** When an in-scope class receives attacker-controlled input and hands it to an unsafe operation in a sibling package or parent project (`com.fasterxml.jackson.core.*`, `org.yaml.snakeyaml.constructor.*`, anything outside the review root), the in-scope class is the attacker's API — file it.  Phrases to reject: "the reflective invoke lives in another jar", "delegates to X in a sibling package — out of scope", "sink is in the JDK / core library".  File at the in-scope class's public method; cite the delegation call site as the sink line; describe the downstream unsafe op in Impact.

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
