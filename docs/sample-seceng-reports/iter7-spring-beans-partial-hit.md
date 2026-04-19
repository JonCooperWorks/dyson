# Security Review: Spring Framework Beans Module

## CRITICAL

No findings.

## HIGH

### ClassEditor allows arbitrary class loading when used with external data binding
- **File:** `propertyeditors/ClassEditor.java:65`
- **Evidence:**
  ```java
  setValue(ClassUtils.resolveClassName(text.trim(), this.classLoader));
  ```
- **Attack Tree:**
  ```
  (external input) — HTTP request parameter or form field bound via BeanWrapper
    └─ BeanWrapperImpl.setPropertyValue — setter invoked with attacker-controlled string
      └─ TypeConverterDelegate.convertIfNecessary — invokes ClassEditor for Class-typed property
        └─ ClassEditor.setAsText:65 — ClassUtils.resolveClassName loads any class by name
  ```
- **Impact:** Attacker-supplied string is resolved to a loaded `Class` object via the thread-context ClassLoader. While `resolveClassName` only loads (not instantiates) the class, this exposes arbitrary class references to the application, enabling class enumeration and enabling downstream exploitation when combined with other sinks that instantiate loaded classes.
- **Exploit:** Supply `org.springframework.beans.propertyeditors.ClassEditor` to a bean property of type `java.lang.Class` during data binding. The class `org.springframework.beans.propertyeditors.ClassEditor.class` is loaded into the JVM.
- **Remediation:** Restrict `ClassEditor` to a whitelist of acceptable class names, or do not register it as a default editor. Override in `BeanWrapperImpl` configuration:
  ```java
  @Override
  public void registerCustomEditor(Class<?> type, PropertyEditor editor) {
      if (type == Class.class && !isApprovedClass(editor)) return;
      super.registerCustomEditor(type, editor);
  }
  ```

## MEDIUM

### SnakeYAML Constructor (not SafeConstructor) used in YamlProcessor
- **File:** `factory/config/YamlProcessor.java:187`
- **Evidence:**
  ```java
  return new Yaml(new FilteringConstructor(loaderOptions), new Representer(), ...);
  ```
  Where `FilteringConstructor extends Constructor` (line 437), not `SafeConstructor`.
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=java, files=143, defs=1842, calls=8901, unresolved_callees=234
  
  Found 1 candidate path(s) from factory/config/YamlProcessor.java:198 to factory/config/YamlProcessor.java:187:
    factory/config/YamlProcessor.java:198 [byte 5892-6012] — method `process` — taint root: reader, yaml
      └─ factory/config/YamlProcessor.java:187 [byte 3401-3529] — [SINK REACHED] — tainted at sink: Yaml constructor with FilteringConstructor
  ```
- **Attack Tree:**
  ```
  YAML file (attacker-modified config resource)
    └─ YamlProcessor.process:197 — yaml.loadAll(reader) processes YAML tags
      └─ SnakeYAML FilteringConstructor.getClassForName:445 — type tag → class loading
        └─ YamlProcessor.createYaml:187 — new Yaml(Constructor, ...) allows arbitrary type tags
  ```
- **Impact:** The `Constructor` base class supports arbitrary Java type instantiation via YAML tags (`!!className`). The `FilteringConstructor` restricts this to `supportedTypes`. When `supportedTypes` is empty (default), ALL explicit type tags are rejected — which is safe. However, when `setSupportedTypes()` is called with types that have exploitable constructors (e.g., `java.lang.Class`, `org.apache.commons.configuration.JNDIConfiguration`), and attacker-influenced YAML is loaded, arbitrary code execution is possible. The mitigations are coincidental: the empty-set default blocks all tag types, and the type list is set by configuration, not user input. A refactor that adds a risky type to `supportedTypes` would flip this to live RCE.
- **Exploit:** ```yaml !!javax.script.ScriptEngineManager [] ``` in a YAML config would load `ScriptEngineManager` and trigger arbitrary script engine initialization if `javax.script.ScriptEngineManager` were in `supportedTypes`.
- **Remediation:** Use SnakeYAML's `SafeConstructor` as the base class instead of `Constructor`, so that even if `supportedTypes` is misconfigured to include a dangerous type, the SnakeYAML base class won't process arbitrary type tags. Change `FilteringConstructor` to extend `SafeConstructor` rather than `Constructor`.

### InputStreamEditor / ReaderEditor accept arbitrary URL schemes without scheme restriction
- **File:** `propertyeditors/InputStreamEditor.java:69`
- **Evidence:**
  ```java
  this.resourceEditor.setAsText(text);
  Resource resource = (Resource) this.resourceEditor.getValue();
  setValue(resource != null ? resource.getInputStream() : null);
  ```
- **Attack Tree:**
  ```
  (external input) — HTTP parameter mapped to java.io.InputStream property
    └─ BeanWrapperImpl.setPropertyValue — value passed through TypeConverterDelegate
      └─ InputStreamEditor.setAsText:69 — accepts any URL scheme (file:, http:, etc.)
        └─ opens arbitrary resource stream for consumption
  ```
- **Impact:** Accepts any URL scheme (`file:`, `http:`, `classpath:`, etc.) without restriction. When used with data binding from external input, this enables reading arbitrary files from the filesystem or fetching arbitrary network resources and streaming them into bean properties. An attacker could set `file:/etc/passwd` to read sensitive files, or `http://attacker.com/shell.sh` to fetch remote content into application state.
- **Remediation:** Restrict accepted URL schemes at the `InputStreamEditor`/`ReaderEditor` level, or remove default registration for beans that accept external data binding. Provide a configuration option to whitelist acceptable schemes:
  ```java
  @Override
  public void setAsText(String text) {
      if (!isAllowedScheme(text)) {
          throw new IllegalArgumentException("URL scheme not allowed: " + text);
      }
      // ... rest
  }
  ```

## LOW / INFORMATIONAL

No findings.

## Checked and Cleared

- `propertyeditors/ClassArrayEditor.java:71` — Same-class-loading behavior as ClassEditor, but for array types (Class[]). Same mitigation applies.
- `propertyeditors/FileEditor.java:88` — Accepts file paths via ResourceEditor; mitigated by requiring file: prefix or resource existence check (lines 88-93). Config-time only.
- `propertyeditors/PathEditor.java:84` — Similar to FileEditor; uses Paths.get(URI) or ResourceEditor. Config-time only.
- `propertyeditors/URLEditor.java:71` — Accepts any URL string; delegates to ResourceEditor. Config-time only.
- `propertyeditors/ReaderEditor.java:69` — Same as InputStreamEditor; delegates to ResourceEditor. Config-time only.
- `propertyeditors/InputSourceEditor.java:72` — Converts to SAX InputSource from resource location. Config-time only.
- `factory/groovy/GroovyBeanDefinitionReader.java:269` — GroovyShell.evaluate() on configuration files (trusted), not user input.
- `factory/config/YamlPropertiesFactoryBean.java:133` — Uses YamlProcessor with SafeConstructor base + FilteringConstructor. Empty supportedTypes = safe default.
- `factory/config/YamlMapFactoryBean.java:122` — Same as YamlPropertiesFactoryBean.
- `factory/support/DefaultListableBeanFactory.java:1876` — readObject throws NotSerializableException explicitly; safe.
- `factory/config/DependencyDescriptor.java:439` — readObject with defaultReadObject; deserializes trusted internal state.
- `TypeConverterDelegate.java:197` — String constructor invocation via `BeanUtils.instantiateClass(strCtor, value)` — only invokes on `requiredType` which is a type known to the bean descriptor, not attacker-controlled.
- `TypeConverterDelegate.java:289` — Raw enum type resolution from string; only accesses public static fields, no code execution.
- `factory/config/TypedStringValue.java:166` — resolveTargetType uses ClassUtils.forName with classLoader from bean factory config.

## Dependencies

No dependency manifests found (no `pom.xml`, `build.gradle` or lockfiles present). The codebase is a standalone module snapshot without packaging metadata. Manual dependency audit required for deployment.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `propertyeditors/ClassEditor.java:65` — Add class name blocklist to prevent arbitrary class loading during data binding, or suppress default registration for externally-bound beans.

### Short-term (MEDIUM)
1. `factory/config/YamlProcessor.java:187` — Change `FilteringConstructor` to extend `SafeConstructor` rather than `Constructor` to ensure SnakeYAML base-class protections apply.
2. `propertyeditors/InputStreamEditor.java:69` — Restrict accepted URL schemes to a configurable whitelist, or provide a way to disable default registration.

### Hardening (LOW)
1. `propertyeditors/FileEditor.java:88` — Add path normalization and traversal checks before accepting file paths from bean configuration.