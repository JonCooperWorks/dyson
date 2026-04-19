Now I have a comprehensive understanding of the codebase. Let me compile the security report.

## CRITICAL

### SQL Injection via ORDER BY clause in server listing endpoint
- **File:** `mitigation/Servers.java:53`
- **Evidence:**
  ```java
  "select id, hostname, ip, mac, status, description from SERVERS where status <> 'out"
      + " of order' order by "
      + column)) {
  ```
- **Attack Tree:**
  ```
  mitigation/Servers.java:45 — HTTP GET /SqlInjectionMitigations/servers?column=...
    └─ mitigation/Servers.java:53 — User-controlled 'column' concatenated into SQL ORDER BY clause
      └─ mitigation/Servers.java:54 — statement.executeQuery() executes injected query
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Java, files=23, defs=101, calls=575, unresolved_callees=0
  
  Found 1 candidate path(s) from mitigation/Servers.java:45 to mitigation/Servers.java:54:
  
  Path 1 (depth 1, resolved 2/2 hops):
    mitigation/Servers.java:45 [byte 1245-1319] — fn `sort` — taint root: column
    └─ mitigation/Servers.java:54 [byte 1663-1712] — [SINK REACHED] — tainted at sink: column
  ```
- **Impact:** Extracts arbitrary data from any table via UNION injection, modifies or deletes data via stacked queries (HSQLDB supports multiple statements). The `column` parameter is appended directly to the ORDER BY clause. Prepared statements cannot parameterize column names, so this remains vulnerable despite using `PreparedStatement`.
- **Exploit:** `GET /SqlInjectionMitigations/servers?column=1%20UNION%20ALL%20SELECT%20*%20FROM%20user_system_data--`
- **Remediation:** Use a whitelist of allowed column names:
  ```java
  private static final Set<String> ALLOWED_COLUMNS = Set.of("id", "hostname", "ip", "mac", "status", "description");
  // ...
  if (!ALLOWED_COLUMNS.contains(column)) {
      throw new IllegalArgumentException("Invalid column: " + column);
  }
  ```

### SQL Injection in user registration — existence check uses string concatenation
- **File:** `advanced/SqlInjectionChallenge.java:55`
- **Evidence:**
  ```java
  String checkUserQuery =
      "select userid from sql_challenge_users where userid = '" + username + "'";
  Statement statement = connection.createStatement();
  ResultSet resultSet = statement.executeQuery(checkUserQuery);
  ```
- **Attack Tree:**
  ```
  advanced/SqlInjectionChallenge.java:45 — HTTP PUT /SqlInjectionAdvanced/register?username_reg=...
    └─ advanced/SqlInjectionChallenge.java:55 — User-controlled 'username' concatenated into SELECT query
      └─ advanced/SqlInjectionChallenge.java:57 — statement.executeQuery(checkUserQuery) executes injected SQL
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Java, files=23, defs=101, calls=575, unresolved_callees=0
  
  Found 1 candidate path(s) from advanced/SqlInjectionChallenge.java:46 to advanced/SqlInjectionChallenge.java:57:
  
  Path 1 (depth 1, resolved 2/2 hops):
    advanced/SqlInjectionChallenge.java:46 [byte 1616-1668] — fn `registerNewUser` — taint root: email, password, username
    └─ advanced/SqlInjectionChallenge.java:57 [byte 2125-2194] — [SINK REACHED] — tainted at sink: username, email, password
  ```
- **Impact:** Extracts arbitrary database contents via UNION-based injection or causes denial of service. Payload `' UNION SELECT password FROM user_system_data-- ` as `username_reg` will leak password data in the response. The INSERT below uses parameterized queries, but the existence check does not.
- **Exploit:** `PUT /SqlInjectionAdvanced/register?username_reg=' UNION SELECT password FROM user_system_data--&email_reg=x&password_reg=x`
- **Remediation:** Use parameterized query for the existence check:
  ```java
  String checkUserQuery = "select userid from sql_challenge_users where userid = ?";
  PreparedStatement checkStmt = connection.prepareStatement(checkUserQuery);
  checkStmt.setString(1, username);
  ResultSet resultSet = checkStmt.executeQuery();
  ```

## HIGH

### SQL Injection in employee search — multiple lesson endpoints
- **File:** `introduction/SqlInjectionLesson8.java:50`
- **Evidence:**
  ```java
  String query =
      "SELECT * FROM employees WHERE last_name = '"
          + name
          + "' AND auth_tan = '"
          + auth_tan
          + "'";
  // ...
  ResultSet results = statement.executeQuery(query);
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson8.java:43 — HTTP POST /SqlInjection/attack8?name=...&auth_tan=...
    └─ introduction/SqlInjectionLesson8.java:50 — User-controlled 'name' and 'auth_tan' concatenated into SQL query
      └─ introduction/SqlInjectionLesson8.java:62 — statement.executeQuery(query) executes injected SQL
  ```
- **Impact:** Extracts all employee records via UNION injection in name parameter. `name=' UNION SELECT * FROM employees --` returns all rows without needing a valid auth_tan.
- **Exploit:** `POST /SqlInjection/attack8` with `name=' UNION SELECT * FROM employees -- &auth_tan=x`
- **Remediation:** Use parameterized query:
  ```java
  String query = "SELECT * FROM employees WHERE last_name = ? AND auth_tan = ?";
  PreparedStatement stmt = connection.prepareStatement(query);
  stmt.setString(1, name);
  stmt.setString(2, auth_tan);
  ```

### SQL Injection in salary update — data integrity violation
- **File:** `introduction/SqlInjectionLesson9.java:51`
- **Evidence:**
  ```java
  String queryInjection =
      "SELECT * FROM employees WHERE last_name = '"
          + name
          + "' AND auth_tan = '"
          + auth_tan
          + "'";
  // ...
  statement.execute(queryInjection);
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson9.java:44 — HTTP POST /SqlInjection/attack9?name=...&auth_tan=...
    └─ introduction/SqlInjectionLesson9.java:51 — User-controlled 'name' and 'auth_tan' concatenated into SQL
      └─ introduction/SqlInjectionLesson9.java:65 — statement.execute(queryInjection) executes injected SQL
  ```
- **Impact:** Modifies employee salary data within the same transaction. Injection like `name=' UPDATE employees SET salary=100000 WHERE auth_tan='3SL99A'--` increases the attacker's salary. Uses `execute()` (not `executeQuery`), so UPDATE statements are executed directly.
- **Exploit:** `POST /SqlInjection/attack9` with `name='; UPDATE employees SET salary=999999 WHERE auth_tan='3SL99A'-- &auth_tan=x`
- **Remediation:** Use parameterized query:
  ```java
  String query = "SELECT * FROM employees WHERE last_name = ? AND auth_tan = ?";
  PreparedStatement stmt = connection.prepareStatement(query);
  stmt.setString(1, name);
  stmt.setString(2, auth_tan);
  ```

### Second-order SQL Injection via query logging function
- **File:** `introduction/SqlInjectionLesson8.java:131-138`
- **Evidence:**
  ```java
  public static void log(Connection connection, String action) {
    action = action.replace('\'', '"');
    // ...
    String logQuery =
        "INSERT INTO access_log (time, action) VALUES ('" + time + "', '" + action + "')";
    statement.executeUpdate(logQuery);
  }
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson8.java:61 — log() called with attacker-controlled query string
    └─ introduction/SqlInjectionLesson8.java:137 — action (malicious SQL) concatenated into INSERT
      └─ introduction/SqlInjectionLesson8.java:142 — statement.executeUpdate(logQuery) executes second-order injection
  ```
- **Impact:** The `log()` function is called with the full attacker-controlled injected query string (line 61 of Lesson8: `log(connection, query)`). Although single quotes are replaced with double quotes (line 132), double quotes can still terminate the string literal in some SQL dialects, and the replacement enables bypass techniques. When `introduction/SqlInjectionLesson9.java` calls this same `log()` function (line 64), injected queries get stored and can trigger second-order injection on subsequent queries reading from `access_log`.
- **Exploit:** Injected query contains `"; DROP TABLE employees;--` which, after quote replacement, becomes `"; DROP TABLE employees;--`. In HSQLDB, this executes after the INSERT.
- **Remediation:** Use parameterized query in the log function:
  ```java
  String logQuery = "INSERT INTO access_log (time, action) VALUES (?, ?)";
  PreparedStatement logStmt = connection.prepareStatement(logQuery);
  logStmt.setString(1, time);
  logStmt.setString(2, action);
  logStmt.executeUpdate();
  ```

### Direct SQL execution with raw query parameter
- **File:** `introduction/SqlInjectionLesson2.java:49`
- **Evidence:**
  ```java
  public AttackResult completed(@RequestParam String query) {
    return injectableQuery(query);
  }
  // ...
  ResultSet results = statement.executeQuery(query);
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson2.java:42 — HTTP POST /SqlInjection/attack2?query=...
    └─ introduction/SqlInjectionLesson2.java:49 — HTTP query parameter passed directly to executeQuery()
      └─ introduction/SqlInjectionLesson2.java:49 — statement.executeQuery(query) executes arbitrary SQL
  ```
- **Impact:** Executes any read-only SQL query. Attacker controls the entire query string, enabling full data extraction from any table, schema enumeration, and privilege escalation.
- **Exploit:** `POST /SqlInjection/attack2` with `query=SELECT password FROM user_system_data`
- **Remediation:** Never pass user-controlled input directly to statement execution. Use parameterized queries with specific column values.

### DML Injection — arbitrary SQL modification via raw query parameter
- **File:** `introduction/SqlInjectionLesson3.java:47`
- **Evidence:**
  ```java
  public AttackResult completed(@RequestParam String query) {
    return injectableQuery(query);
  }
  // ...
  statement.executeUpdate(query);
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson3.java:37 — HTTP POST /SqlInjection/attack3?query=...
    └─ introduction/SqlInjectionLesson3.java:47 — HTTP query parameter passed directly to executeUpdate()
      └─ introduction/SqlInjectionLesson3.java:47 — statement.executeUpdate(query) executes arbitrary DML/DDL
  ```
- **Impact:** Executes any modifying SQL (UPDATE, INSERT, DELETE, DDL). Attacker can modify or destroy any table data. More dangerous than read-only injection since DDL statements (DROP, ALTER, CREATE) can restructure the database schema.
- **Exploit:** `POST /SqlInjection/attack3` with `query=DROP TABLE employees`
- **Remediation:** Never pass user-controlled input directly to statement execution.

### Table DROP via availability attack
- **File:** `introduction/SqlInjectionLesson10.java:49`
- **Evidence:**
  ```java
  String query = "SELECT * FROM access_log WHERE action LIKE '%" + action + "%'";
  // ...
  ResultSet results = statement.executeQuery(query);
  ```
- **Attack Tree:**
  ```
  introduction/SqlInjectionLesson10.java:43 — HTTP POST /SqlInjection/attack10?action_string=...
    └─ introduction/SqlInjectionLesson10.java:49 — User input concatenated into LIKE query
      └─ introduction/SqlInjectionLesson10.java:56 — statement.executeQuery(query) executes injected SQL
  ```
- **Impact:** DROP TABLE via `'; DROP TABLE access_log;--`. The success condition checks if the table was dropped, confirming that DDL operations can be executed through this endpoint.
- **Exploit:** `POST /SqlInjection/attack10` with `action_string='; DROP TABLE access_log;--`
- **Remediation:** Use parameterized query:
  ```java
  String query = "SELECT * FROM access_log WHERE action LIKE ?";
  PreparedStatement stmt = connection.prepareStatement(query);
  stmt.setString(1, "%" + action + "%");
  ```

## MEDIUM

### Ineffective keyword-based input validation bypass
- **File:** `mitigation/SqlOnlyInputValidationOnKeywords.java:37`
- **Evidence:**
  ```java
  userId = userId.toUpperCase().replace("FROM", "").replace("SELECT", "");
  if (userId.contains(" ")) {
      return failed(this).feedback("SqlOnlyInputValidationOnKeywords-failed").build();
  }
  ```
- **Attack Tree:**
  ```
  mitigation/SqlOnlyInputValidationOnKeywords.java:35 — HTTP POST parameter userid_sql_only_input_validation_on_keywords=...
    └─ mitigation/SqlOnlyInputValidationOnKeywords.java:37 — Keyword filtering: "FROM" and "SELECT" removed after uppercasing
      └─ advanced/SqlInjectionLesson6a.java (via lesson6a.injectableQuery) — Bypassed input reaches SQL injection sink
  ```
- **Impact:** The keyword filter removes "FROM" and "SELECT" only once. Nested payloads like `SESELECTLECT` become `SELECT` after one removal, or payloads using `FrOm` with mixed casing may bypass if the toUpperCase is order-dependent. More critically, UNION-based injection without needing FROM/SELECT keywords (e.g., `' UNION ALL SELECT ...` — but SELECT is filtered) can use alternative injection techniques. The space check after keyword removal enables bypass with tab or other whitespace characters.
- **Remediation:** Do not rely on keyword blacklisting. Use parameterized queries (which lesson6a should use but doesn't).

## LOW / INFORMATIONAL

No additional low/informational findings.

## Checked and Cleared

- `advanced/SqlInjectionChallengeLogin.java:34-35` — Uses parameterized query (`?` placeholders) for login. No SQL injection.
- `mitigation/SqlInjectionLesson13.java:45-48` — Uses parameterized query (`?` placeholder) for IP lookup. No SQL injection.
- `mitigation/SqlInjectionLesson10a.java:29-54` — Quiz-style string matching endpoint, no database interaction.
- `mitigation/SqlInjectionLesson10b.java:44-87` — Code submission grader using Java compiler API. No direct SQL execution. Uses regex-based pattern matching, not injection-sink.
- `advanced/SqlInjectionQuiz.java:30-65` — Quiz answer comparison logic, no database interaction.
- `introduction/SqlInjection.java:12-15` — Lesson metadata class only, no request handling.
- `advanced/SqlInjectionAdvanced.java:12-15` — Lesson metadata class only, no request handling.

## Dependencies

NO_MANIFESTS_FOUND — No `pom.xml`, `build.gradle`, or lockfiles found. The codebase consists of raw Java source files without build manifests. Dependency risk cannot be assessed.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `mitigation/Servers.java:53` — Whitelist allowed column names for ORDER BY
2. `advanced/SqlInjectionChallenge.java:55` — Parameterize the user existence check query
3. `introduction/SqlInjectionLesson8.java:50` — Parameterize employee search query
4. `introduction/SqlInjectionLesson9.java:51` — Parameterize salary update query
5. `introduction/SqlInjectionLesson8.java:137` — Parameterize log INSERT query
6. `introduction/SqlInjectionLesson2.java:49` — Replace raw query execution with parameterized queries
7. `introduction/SqlInjectionLesson3.java:47` — Replace raw query execution with parameterized queries
8. `introduction/SqlInjectionLesson10.java:49` — Parameterize LIKE query with wildcard binding