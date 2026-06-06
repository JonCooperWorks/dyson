Starting points for C# / .NET — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `Process.Start(new ProcessStartInfo { FileName = user, ... })` — RCE.
- `Process.Start("cmd.exe", "/c " + user)` — RCE via shell flag.
- `UseShellExecute = true` with a user-controlled FileName → shell-expansion.

**Reflection (.NET's prototype-walk primitive)**
- `Type.GetType(user_name)` / `Assembly.Load(user_bytes).GetType(name).InvokeMember(...)` — class-from-string + invocation.
- `Activator.CreateInstance(Type.GetType(user))` — RCE.
- `MethodInfo.Invoke(obj, args)` with attacker-selected method.
- Dynamic: `dynamic x = obj; x.SomeMethod();` — at runtime turns into Reflection.

**Deserialization**
- `BinaryFormatter.Deserialize(stream)` — MARKED OBSOLETE, still widely used; RCE on untrusted bytes.
- `ObjectStateFormatter`, `LosFormatter`, `NetDataContractSerializer`, `SoapFormatter` — all insecure on untrusted input.
- `XmlSerializer` is type-safe IF the target type is fixed; `DataContractSerializer` with `KnownTypes` unioned from user input is exploitable.
- `Newtonsoft.Json` `JsonConvert.DeserializeObject(json, settings)` with `TypeNameHandling.All` / `.Auto` / `.Objects` — polymorphic deserialization RCE.
- System.Text.Json with a `JsonSerializerOptions { TypeInfoResolver = PolymorphicResolver }` and attacker-controlled discriminator.
- `YamlDotNet` with a naive `Deserializer()` on untrusted input — type injection.

**SQL injection**
- `SqlCommand cmd = new SqlCommand("SELECT ... '" + user + "'", conn)` — use `SqlParameter` / `AddWithValue`.
- Dapper `conn.Query($"... {user}")` — interpolated strings in Dapper don't parameterise automatically; use `new { param = user }`.
- EF Core `db.Users.FromSqlRaw($"... {user}")` — use `FromSqlInterpolated` (parameterises).
- `DbContext.Database.ExecuteSqlRaw("... " + user)`.

**Path / file**
- `File.ReadAllBytes(user)`, `File.Open(user, ...)` — traversal unless `Path.GetFullPath` + prefix check.
- `Path.Combine(base, user)` does NOT prevent `..`; use `GetFullPath(Combine(base, user)).StartsWith(GetFullPath(base))`.
- ASP.NET Core `PhysicalFile(user)` / `Results.File(user)` in endpoints.
- ZipArchiveEntry with naive `Path.Combine(dest, entry.FullName)` — Zip Slip.

**XSS / templates**
- Razor: `@Html.Raw(user)` — bypasses HTML encoding.
- `@Html.DisplayFor` / `@user` — encoded by default; switch to Raw is the finding.
- Blazor: `MarkupString(user)` — same pattern.

**XML / XXE**
- `XmlReader.Create(stream)` — defaults vary by framework; set `XmlReaderSettings { DtdProcessing = DtdProcessing.Prohibit }`.
- `XmlDocument.LoadXml` / `XPathDocument` — similar.

**Crypto / randomness**
- `System.Random` for tokens — predictable.  Use `RandomNumberGenerator.Create()`.
- `MD5`, `SHA1` for password hashing — use `Rfc2898DeriveBytes` (PBKDF2), `Argon2`, `BCrypt`.
- `ECB` cipher mode (`CipherMode.ECB`) — never correct.
- `string.Equals` for HMAC comparison — timing-unsafe; use `CryptographicOperations.FixedTimeEquals`.

**Open redirect / SSRF**
- `Redirect(user_url)` in ASP.NET — `Url.IsLocalUrl(user)` gate; returning `LocalRedirect(user)` is safer.
- `HttpClient.GetAsync(user_url)` without host allowlist.

## Tree-sitter seeds (c-sharp)

```scheme
; Process.Start / Runtime exec
(invocation_expression
  function: (member_access_expression name: (identifier) @m)
  (#match? @m "^(Start|ExecuteScalar|ExecuteReader|ExecuteNonQuery)$"))

; Reflection entry
(invocation_expression
  function: (member_access_expression name: (identifier) @m)
  (#match? @m "^(GetType|Load|CreateInstance|InvokeMember|Invoke|GetMethod)$"))

; Deserializer calls
(invocation_expression
  function: (member_access_expression name: (identifier) @m)
  (#match? @m "^(Deserialize|DeserializeObject|ReadObject)$"))
```
