# Security Review: Keycloak Admin Services

## HIGH

### Missing Authorization Check on `testSMTPConnection` Endpoint

- **File:** `RealmAdminResource.java:989`
- **Evidence:**
  ```java
  public Response testSMTPConnection(Map<String, String> settings) throws Exception {
      try {
          UserModel user = auth.adminAuth().getUser();
  ```

The `testSMTPConnection` method (line 989) only calls `auth.adminAuth().getUser()` but does NOT call `auth.realm().requireManageRealm()` or any other authorization check. Compare to other methods in the same class:

- `updateRealm` (line 414): calls `auth.realm().requireManageRealm()`
- `deleteRealm` (line 479): calls `auth.realm().requireManageRealm()`
- `logoutAll` (line 607): calls `auth.users().requireManage()`
- `deleteSession` (line 626): calls `auth.users().requireManage()`
- `clearEvents` (line 942): calls `auth.realm().requireManageEvents()`

The only authorization check on this method is `auth.adminAuth().getUser()` which retrieves the current authenticated admin user but does not verify any realm-level management permissions.

- **Attack Tree:**
  ```
  RealmAdminResource.java:989 — testSMTPConnection POST endpoint
    └─ auth.adminAuth().getUser() — only retrieves current user, no permission check
      └─ session.getProvider(EmailTemplateProvider.class).sendSmtpTestEmail(settings, user) — sends email using user-controlled settings
  ```
  
  Any authenticated admin user can access this endpoint (they must have passed the global admin authentication at `AdminRoot.java:218`) but they do not need manage-realm permission.

- **Impact:** An authenticated user with minimal admin access (e.g. view-only) can send arbitrary emails through the email server configured for this realm. This allows:
  - Sending emails to arbitrary recipients (the `user` parameter's email is used as the recipient)
  - Using arbitrary SMTP servers, ports, credentials, and connection properties
  - Potentially causing information disclosure or harassment by spamming users with test emails
  - The sender email address is derived from the admin user's email, so it appears to come from a trusted admin
  - This can also be abused to perform SMTP server discovery and enumeration if the attacker controls the user account's email

- **Exploit:**
  Any authenticated admin user can send a POST request:
  ```bash
  curl -X POST "https://keycloak.example.com/admin/realms/master/testSMTPConnection" \
    -H "Authorization: Bearer <admin_token>" \
    -H "Content-Type: application/json" \
    -d '{"host":"smtp.example.com","port":"587","from":"admin@example.com"}'
  ```

- **Remediation:** Add the appropriate permission check:
  ```java
  public Response testSMTPConnection(Map<String, String> settings) throws Exception {
      auth.realm().requireManageRealm();  // ADD THIS LINE
      try {
          UserModel user = auth.adminAuth().getUser();
  ```

## LOW

### Stack Trace Leak in Admin Event Endpoint

- **File:** `RealmAdminResource.java:1000-1002`
- **Evidence:**
  ```java
  } catch (Exception e) {
      e.printStackTrace();
      logger.errorf("Failed to send email \n %s", e.getCause());
  ```
  
  The `e.printStackTrace()` prints the full stack trace to stderr. While this should not reach the client (an error JSON is returned), it may leak internal class names and method signatures to stderr which could be logged by an attacker with access to logs or if output is visible in debug mode.

- **Impact:** Internal class names, method signatures, and call chains leak to stderr. This is informational only as it does not directly expose this data to the attacker.

- **Remediation:** Remove `e.printStackTrace()` and rely only on the logger call with appropriate log level.

## Checked and Cleared

- `RealmAdminResource.java:414` — `updateRealm` calls `requireManageRealm()` and validates key pairs and certificates with `KeyPairVerifier.verify()` and `PemUtils.decodeCertificate()`
- `RealmAdminResource.java:422-423` — `ReservedCharValidator.validate()` validates realm names and locales
- `RealmAdminResource.java:771-784` — Event queries use the typed `EventQuery` API with parameterized methods, not string concatenation
- `RealmAdminResource.java:859-895` — Admin event queries use the typed `AdminEventQuery` API with parameterized methods
- `UserResource.java:336` — `impersonate()` calls `auth.users().requireImpersonate(user)`
- `UserResource.java:343-351` — Impersonation validates user status and service account restrictions
- `IdentityProvidersResource.java:119` — `importFrom()` requires `requireManageIdentityProviders()` before processing uploaded files
- `IdentityProvidersResource.java:145` — URL import requires `requireManageIdentityProviders()`
- `RealmLocalizationResource.java:74` — All localization write operations require `requireManageRealm()`
- `AdminRoot.java:160-190` — Admin authentication validates the JWT token with `AppAuthManager.BearerTokenAuthenticator`
- `AdminAuth.java:62-69` — `hasRealmRole()` properly checks both user role and client scope
- `AuthenticationManagementResource.java` — All flow management operations require `requireManageRealm()`
- `ComponentResource.java` — Component CRUD operations require `requireManageRealm()`
- `UserResource.java:402-403` — Session retrieval requires `requireView(user)` check
- `UserResource.java:624-631` — Delete user requires `requireManage()` check
- `UserResource.java:849` — `executeActionsEmail()` requires `requireManage()` check
- `UserResource.java:941` — `sendVerifyEmail()` requires `requireManage()` check
- `ClientResource.java:139` — Client update requires `requireManage()` check
- `ClientResource.java:231` — Client delete requires `requireManage()` check
- `GroupResource.java:109` — Group update requires auth check
- `RoleContainerResource.java:137` — Create role requires `auth.roles().requireManage()`
- `RealmsAdminResource.java:133` — `importRealm` requires `auth.realm().requireManageRealm()`

## Dependencies

No dependency manifests (`pom.xml`, `build.gradle`, etc.) were found in the review scope. The dependency review could not assess third-party library vulnerabilities. This is a single directory of source files from the Keycloak admin-services module.

## Remediation Summary

### Immediate (HIGH)
1. `RealmAdminResource.java:989` — Add `auth.realm().requireManageRealm()` check at the beginning of the `testSMTPConnection(Map<String, String>)` method

### Hardening (LOW)
1. `RealmAdminResource.java:1000` — Remove `e.printStackTrace()` and rely solely on structured logging