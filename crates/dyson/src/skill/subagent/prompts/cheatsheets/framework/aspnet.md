Starting points for ASP.NET Core — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`[FromBody] T`, `[FromQuery] T`, `[FromRoute] T`, `[FromHeader]`, `[FromForm]`, `HttpContext.Request.*`, model-bound action parameters.

## Sinks

**SQL injection**
- `dbContext.Database.ExecuteSqlRaw($"DELETE FROM t WHERE id = {user}")` — interpolated string is concat, NOT parameterised.  Use `ExecuteSqlInterpolated($"...")` (this form IS parameterised) or `ExecuteSqlRaw("...", new[] { param })`.
- `dbContext.Users.FromSqlRaw($"... {user}")` — same.
- Dapper `conn.Query($"SELECT * FROM t WHERE id = {user}")` — Dapper's `$"..."` is not parameterised; use `new { user }`.
- `SqlCommand cmd = new SqlCommand("SELECT ... '" + user + "'", conn)`.

**Mass assignment**
- `public IActionResult Create([FromBody] User u)` — every JSON field sets the entity.  Use a DTO with only allowed fields; map into entity server-side.
- `TryUpdateModelAsync(entity, prefix, includeProperties: null)` — `null` = all properties; attacker sets anything.

**Deserialization**
- `BinaryFormatter.Deserialize(stream)` — obsolete but still found; RCE on untrusted bytes.
- `Newtonsoft.Json` `TypeNameHandling.All` / `.Auto` / `.Objects` — polymorphic RCE.
- `System.Text.Json` with custom `JsonConverter` resolving types from a discriminator string — polymorphic RCE if the converter walks an arbitrary type registry.
- `XmlSerializer` with `KnownTypes` built from untrusted input.

**Command execution**
- `Process.Start(new ProcessStartInfo { FileName = user, UseShellExecute = true })` — shell.
- `Process.Start("cmd.exe", "/c " + user)` — shell flag.

**Path / file**
- `PhysicalFile(userPath, "application/octet-stream")` — serves arbitrary file; traversal.
- `File.ReadAllBytes(userPath)`, `File(System.IO.File.OpenRead(userPath), ...)` — traversal.
- `IFormFile.FileName` — attacker-supplied; `Path.Combine(uploadDir, file.FileName)` without `Path.GetFileName` + anchor check = traversal.

**SSRF**
- `HttpClient.GetAsync(userUrl)` without host allowlist.
- `WebRequest.Create(userUrl)` — no default redirect cap to internal addresses.

**Redirect / XSS**
- `Redirect(userUrl)` — use `LocalRedirect(userUrl)` + `Url.IsLocalUrl` gate.
- Razor `@Html.Raw(user)` — XSS.  `@user` is HTML-encoded.
- Blazor `@((MarkupString)user)` — XSS.
- `Content(user, "text/html")` — raw HTML body.

**Authentication / authorization**
- Missing `[Authorize]` / `[Authorize(Roles = "Admin")]` on a state-changing controller.
- `[AllowAnonymous]` on an endpoint that shouldn't be.
- `options.RequireHttpsMetadata = false` in JWT bearer config — accepts tokens over HTTP.
- `TokenValidationParameters { ValidateAudience = false, ValidateIssuer = false, ValidateLifetime = false }` — effectively accepts any signed token.

**CSRF**
- `[IgnoreAntiforgeryToken]` on a state-changing controller accessed via cookie auth — finding.
- MVC with cookie auth and no `@Html.AntiForgeryToken()` in forms.

**Secrets in source**
- `appsettings.json` with plaintext `ConnectionStrings:DefaultConnection` including prod passwords.
- `JwtOptions:SecretKey = "dev-secret"` — signing-key leak enables token forgery.

## Tree-sitter seeds (c-sharp)

```scheme
; [FromBody] / [FromQuery] / etc. — route source markers
(attribute
  name: (identifier) @a
  (#match? @a "^(FromBody|FromQuery|FromRoute|FromHeader|FromForm)$"))

; ExecuteSqlRaw / FromSqlRaw / Query$"..."
(invocation_expression
  function: (member_access_expression name: (identifier) @m)
  (#match? @m "^(ExecuteSqlRaw|FromSqlRaw|ExecuteSqlInterpolated|FromSqlInterpolated|Query|Execute)$"))

; Deserialize / DeserializeObject
(invocation_expression
  function: (member_access_expression name: (identifier) @m)
  (#match? @m "^(Deserialize|DeserializeObject|ReadObject)$"))
```
