# Security Review: Apache Tomcat AJP Connector

## CRITICAL

### AJP Attribute Injection — Arbitrary Request Attribute Setting (Ghostcat)
- **File:** `AjpProcessor.java:709-733`
- **Evidence:**
  ```java
            case Constants.SC_A_REQ_ATTRIBUTE :
                requestHeaderMessage.getBytes(tmpMB);
                String n = tmpMB.toString();
                requestHeaderMessage.getBytes(tmpMB);
                String v = tmpMB.toString();
                /*
                 * AJP13 misses to forward the local IP address and the remote port. Allow the AJP connector to add this info via
                 * private request attributes.
                 * We will accept the forwarded data and remove it from the
                 * public list of request attributes.
                 */
                if(n.equals(Constants.SC_A_REQ_LOCAL_ADDR)) {
                    request.localAddr().setString(v);
                } else if(n.equals(Constants.SC_A_REQ_REMOTE_PORT)) {
                    try {
                        request.setRemotePort(Integer.parseInt(v));
                    } catch (NumberFormatException nfe) {
                        // Ignore invalid value
                    }
                } else if(n.equals(Constants.SC_A_SSL_PROTOCOL)) {
                    request.setAttribute(SSLSupport.PROTOCOL_VERSION_KEY, v);
                } else {
                    request.setAttribute(n, v );
                }
                break;
  ```
- **Attack Tree:**
  ```
  <AJP client:8009> — sends SC_A_REQ_ATTRIBUTE (0x0A) with arbitrary (name, value) pair over raw AJP13 socket
    └─ AjpProcessor.java:704 — attributeCode byte read as 0x0A, enters SC_A_REQ_ATTRIBUTE case
      └─ AjpProcessor.java:710-713 — name (n) and value (v) read from AJP payload with no sanitization
        └─ AjpProcessor.java:732 — request.setAttribute(n, v) called — arbitrary attribute set on the servlet request
          └─ AjpProcessor.java:399 — getAdapter().service(request, response) — tainted request passed into servlet pipeline
  ```
- **Impact:** Attacker with network access to the AJP port sets arbitrary servlet request attributes. This enables authentication bypass by spoofing `javax.servlet.request.X509Certificate`, privilege elevation via security-relevant attributes, and information disclosure through application logic that trusts request attributes. Downstream servlet filters and the servlet container itself may honor these attributes for authorization decisions.
- **Exploit:** Connect to port 8009, send a crafted AJP13 packet with attribute code `0x0A`, name `javax.servlet.request.X509Certificate` (or any security-relevant attribute), and an attacker-controlled value. A raw AJP13 FORWARD_REQUEST packet can be constructed with:

  ```
  0x12 0x34 <len> 0x02 <method> <uri> <remote-addr> <remote-host> <server-name> <port> <ssl-flag>
  <headers...>
  0xFF 0x0A <name-len> "javax.servlet.request.X509Certificate" <null> <value-len> "<fake-cert>" <null>  <terminator> 0xFF
  ```

- **Remediation:** Block all attribute names that begin with `javax.servlet.request.` or `jakarta.servlet.request.` in the `SC_A_REQ_ATTRIBUTE` case. Add the following check before line 732:
  ```java
                } else {
                    // CVE-2020-1938: reject servlet security attributes from AJP
                    if (n.startsWith("jakarta.servlet.request.") ||
                        n.startsWith("javax.servlet.request.")) {
                        // Ignore — security attributes must come from the container
                        break;
                    }
                    request.setAttribute(n, v);
  ```

## HIGH

### Remote User Impersonation When `tomcatAuthentication=false`
- **File:** `AjpProcessor.java:746-755`
- **Evidence:**
  ```java
            case Constants.SC_A_REMOTE_USER :
                boolean tomcatAuthorization  = protocol.getTomcatAuthorization();
                if (tomcatAuthorization || !protocol.getTomcatAuthentication()) {
                    // Implies tomcatAuthentication == false
                    requestHeaderMessage.getBytes(request.getRemoteUser());
                    request.setRemoteUserNeedsAuthorization(tomcatAuthorization);
                } else {
                    // Ignore user information from reverse proxy
                    requestHeaderMessage.getBytes(tmpMB);
                }
                break;
  ```
- **Attack Tree:**
  ```
  <AJP client:8009> — sends SC_A_REMOTE_USER (0x03) attribute with arbitrary username over raw AJP13 socket
    └─ AjpProcessor.java:746 — enters SC_A_REMOTE_USER case
      └─ AjpProcessor.java:748 — condition `!protocol.getTomcatAuthentication()` is true when configured
        └─ AjpProcessor.java:750 — request.getRemoteUser() set directly from AJP payload (no validation)
          └─ AjpProcessor.java:399 — getAdapter().service(request, response) — servlet trusts getRemoteUser() and grants access
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Any client with network access to the AJP port impersonates any user. When `tomcatAuthentication=false` (a common configuration when placing Apache httpd or nginx in front of Tomcat for authentication), the servlet container trusts the AJP-sent remote user unconditionally. `request.getRemoteUser()` is used by servlet containers for JAAS login, `HttpServletRequest.isUserInRole()` checks, and application-level access control.
- **Exploit:** Send an AJP13 FORWARD_REQUEST packet to port 8009 with attribute code `0x03` (SC_A_REMOTE_USER) followed by the target username (e.g., "admin"). No password or credential required.
- **Remediation:** This is a design limitation of AJP. Mitigate by: (1) always using `tomcatAuthentication=true` (default), (2) restricting AJP port access to the frontend web server via firewall rules, or (3) deploying the `requiredSecret` mechanism (AbstractAjpProtocol.java:142) combined with a strong secret to prevent unauthorized AJP clients.

### Authentication Bypass via `SC_A_AUTH_TYPE` Spoofing
- **File:** `AjpProcessor.java:758-764`
- **Evidence:**
  ```java
            case Constants.SC_A_AUTH_TYPE :
                if (protocol.getTomcatAuthentication()) {
                    // ignore server
                    requestHeaderMessage.getBytes(tmpMB);
                } else {
                    requestHeaderMessage.getBytes(request.getAuthType());
                }
                break;
  ```
- **Attack Tree:**
  ```
  <AJP client:8009> — sends SC_A_AUTH_TYPE (0x04) attribute with "Basic" or other auth type
    └─ AjpProcessor.java:758 — enters SC_A_AUTH_TYPE case
      └─ AjpProcessor.java:759 — condition `protocol.getTomcatAuthentication()` is false
        └─ AjpProcessor.java:763 — request.getAuthType() set to attacker-controlled value
          └─ Downstream — servlet authorization logic trusts auth type to grant access
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Attacker sets the authentication type to any value (e.g., "Basic", "DIGEST", "CLIENT_CERT"), enabling downstream servlet filters to believe a legitimate authentication mechanism has been applied. Combined with remote user spoofing above, provides full authentication bypass.
- **Exploit:** Same as Remote User Impersonation, send attribute code `0x04` with value "Basic" alongside the spoofed username at code `0x03`.
- **Remediation:** Same remediation as Remote User Impersonation — use `tomcatAuthentication=true`, or restrict AJP port via network-level access control.

## MEDIUM

### Unvalidated `localName` Enables Host Header Injection via AJP
- **File:** `AjpProcessor.java:632`
- **Evidence:**
  ```java
        requestHeaderMessage.getBytes(request.localName());
  ```
  Later populated at line 872:
  ```java
            request.serverName().duplicate(request.localName());
  ```
- **Attack Tree:**
  ```
  <AJP client:8009> — sends forged localName in AJP13 FORWARD_REQUEST packet
    └─ AjpProcessor.java:632 — localName set from unvalidated AJP payload
      └─ AjpProcessor.java:872 — serverName duplicates the forged localName
        └─ AjpProcessor.java:399 — getAdapter().service(request, response) — servlet uses getServerName() for URL generation, redirects, password reset token URLs
  ```
- **Taint Trace:** not run within budget — same-line / structural evidence only
- **Impact:** Attacker controls `request.getServerName()`. If the servlet uses this value for URL construction (password reset links, OAuth redirect URIs, canonical URLs), it enables cache poisoning, credential theft, and open-redirect-class attacks. Requires network access to the AJP port.
- **Remediation:** Validate `localName` against an allowlist of expected hostnames, or use the `Host` header from the front-end proxy rather than the AJP-encoded local name.

### Unvalidated `localPort` via `setLocalPort` Accepts Arbitrary Integer
- **File:** `AjpProcessor.java:633`
- **Evidence:**
  ```java
        request.setLocalPort(requestHeaderMessage.getInt());
  ```
- **Attack Tree:**
  ```
  <AJP client:8009> — sends arbitrary 16-bit port value in AJP13 packet
    └─ AjpProcessor.java:633 — setLocalPort accepts any integer from the wire
      └─ AjpProcessor.java:889 — request.setServerPort(request.getLocalPort()) propagates forged port
        └─ Downstream — servlet may use getLocalPort() for URL construction or logging
  ```
- **Impact:** Attacker controls the server port as seen by the servlet. Combined with localName spoofing above (MEDIUM), enables full URL manipulation. The `getInt()` call at line 633 reads 2 bytes, accepting any 0-65535 value. On its own, the port value has limited direct impact.
- **Remediation:** Validate port against expected connector port, or trust the port from the front-end proxy configuration.

## LOW / INFORMATIONAL

### Timing-Side-Channel in Secret Comparison
- **File:** `AjpProcessor.java:806`
- **Evidence:**
  ```java
                    if (!tmpMB.equals(requiredSecret)) {
  ```
- **Analysis:** String equality comparison is character-by-character and returns early on first mismatch. A timing-accurate attacker on the same network segment could recover the secret byte-by-byte. However, AJP port access is typically internal-only, and `requiredSecret` is null by default.
- **Remediation:** Use constant-time comparison if `requiredSecret` is relied upon as a primary defense:
  ```java
  if (!MessageBytes.constantTimeEquals(tmpMB, requiredSecret)) {
  ```

## Checked and Cleared

- `AjpMessage.java:349-366` — `processHeader()` validates the magic marker (`0x1234` for incoming, `0x4142` for outgoing). Invalid markers return `-1` and the caller throws `IOException`. Proper input validation.
- `AjpMessage.java:386-392` — `validatePos()` checks read position against message length + 4. Bounds checking is correct; out-of-bounds reads throw `ArrayIndexOutOfBoundsException`.
- `AjpMessage.java:256-266` — `checkOverflow()` validates write position against buffer size. Prevents buffer overrun on append operations.
- `AjpProcessor.java:335-361` — Message type validation: only `JK_AJP13_CPING_REQUEST` and `JK_AJP13_FORWARD_REQUEST` are accepted. Unknown message types close the connection immediately.
- `AjpProcessor.java:541-558` — Message length validated against buffer size. Over-length messages throw `IllegalArgumentException` and are handled by the service loop's catch block.
- `AjpProcessor.java:679-690` — Content-Length header parsed with duplicate-detection logic. Second Content-Length header triggers 400 response and connection close.
- `AbstractAjpProtocol.java:114` — `tomcatAuthentication` defaults to `true`. This is the safe default; authentication forgery findings only apply when an admin explicitly sets it to `false`.
- `AbstractAjpProtocol.java:142-153` — `requiredSecret` mechanism provides optional shared-secret authentication for AJP connections.
- `AbstractAjpProtocol.java:159-167` — `packetSize` enforced at minimum `Constants.MAX_PACKET_SIZE` (8192). No overflow from undersized packets.
- `Constants.java:143-145` — `getMethodForCode()` does not validate `code` bounds; could throw `ArrayIndexOutOfBoundsException`. However, this is inside `prepareRequest()` at AjpProcessor.java line 623, which is caught by the try-catch at lines 379-385. Results in 500 response, no exploit path.
- `AjpNioProtocol.java`, `AjpNio2Protocol.java`, `AjpAprProtocol.java` — Thin subclasses with no security-relevant logic.

## Dependencies

No dependency manifests (`pom.xml`, `build.gradle`, `package.json`, lockfiles, or CycloneDX/SPDX SBOMs) found in the codebase. This directory contains only the AJP protocol handler Java source files. No `dependency_scan` or `dependency_review` applicable.

Note: This code appears to be extracted from Apache Tomcat source. Known CVEs apply to the full Tomcat distribution (e.g., CVE-2020-1938 Ghostcat is documented in the CRITICAL finding above), but version-specific dependency vulnerabilities cannot be assessed without the project's build configuration.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `AjpProcessor.java:732` — Block `javax.servlet.request.*`/`jakarta.servlet.request.*` attribute names in `SC_A_REQ_ATTRIBUTE` case to prevent authentication bypass via attribute injection.
2. `AjpProcessor.java:746-764` — Deprecate or restrict `tomcatAuthentication=false` mode; document that it allows arbitrary remote-user/auth-type forgery from any AJP-connected client. Enforce `requiredSecret` as mandatory when `tomcatAuthentication=false`.
3. `AjpProcessor.java:758-764` — Same as #2: `SC_A_AUTH_TYPE` spoofing vector is eliminated by the same fix.

### Short-term (MEDIUM)
1. `AjpProcessor.java:632` — Validate `localName` against expected connector hostnames or defer to the front-end proxy's Host header.
2. `AjpProcessor.java:633` — Validate `localPort` against expected connector port.

### Hardening (LOW)
1. `AjpProcessor.java:806` — Use constant-time string comparison for `requiredSecret` validation.