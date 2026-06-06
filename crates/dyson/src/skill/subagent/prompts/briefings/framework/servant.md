Starting points for Servant (Haskell web framework) — not exhaustive. Type-level API specification closes a lot; the bugs hide in handlers + custom combinators. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
Handler types pull validated values from the request:
- `QueryParam "k" Text`, `Capture "id" Int`, `Header "H" Text`, `ReqBody '[JSON] MyDto`, `Header "Cookie" Text` parsed manually.

Servant validates parsers via `FromHttpApiData` / `FromJSON`.  A `QueryParam "k" Text` that's missing returns a 400; a `Maybe Text` accepts missing.  Type-level is the first filter — after parsing, the handler gets the value.

## Sinks

**SQL (postgresql-simple / persistent / esqueleto)**
- `query_ conn (Query (BS.pack ("SELECT ... " ++ show user)))` — interpolated query text, SQLi.  Use `query conn "... ?" (Only user)`.
- `esqueleto` typed queries are safe.  `rawSql "... #{user}" []` with user interpolation — SQLi.

**Deserialization**
- `decode :: FromJSON a => ByteString -> Maybe a` into a fixed type is safe.  Into `Value` + walking keys = prototype-walk Haskell analogue.
- `store` / `cereal` / `binary` decoders on untrusted bytes — `decodeOrFail` returns `Left` on failure but CAN succeed with absurd values that break later pattern matches (partial-function landmines).

**Command execution**
- `System.Process.callCommand user` — shell; RCE.  Use `proc "/bin/bin" [args]` + `createProcess`.
- `System.Posix.Process.executeFile user False args env` — RCE if `user` is the program path.

**Unsafe exits**
- `unsafePerformIO` reaching a handler path with user data — bypasses the type system.
- `Data.ByteString.Unsafe.unsafeUseAsCString` + FFI with user-sized length — OOB.

**File / path**
- `readFile user` — traversal.
- Servant's `Raw` combinator for serving files: `serveDirectoryWebApp userRoot` — attacker-derived root.

**Redirect**
- Servant's `Header "Location" Text :> ... :> Get '[...] (Headers '[...] ...)` — `Location` header value.  An attacker-controlled value makes an open redirect.

**Authentication**
- `AuthProtect "jwt-auth"` — custom auth context.  The `AuthHandler` implementation is on the developer; a handler returning `Authenticated user` on any token = bypass.
- `BasicAuthCheck :: BasicAuthData -> IO (BasicAuthResult User)` returning `Authorized user` unconditionally = bypass.

**CORS (servant-cors / wai-cors)**
- `simpleCorsResourcePolicy` is permissive; customise via `corsRequestHeaders`, `corsOrigins = Just ([origin], True)` — `True` credentials + permissive `origins` = credentialed wildcard.

**Error handling**
- `throwError err500 { errBody = BS.pack (show err) }` — `show err` may include secrets.  Use a fixed message + log server-side.

**Servant-server internals**
- Middleware composition via WAI: `liftIO $ someAction` in a handler that leaks thread-local state across requests.

## Tree-sitter seeds (haskell, Servant-focused)

Servant types are at the type level; structural queries in tree-sitter-haskell are limited.  Prefer `ast_describe` on a representative snippet.

```scheme
; top-level binding with Server / ServerT type signature
(signature
  (variable) @v
  (#match? @v "^(server|handler|app)$"))

; callCommand / readFile / decode
(variable) @f (#match? @f "^(callCommand|readFile|writeFile|decode|unsafePerformIO|unsafeCoerce)$")
```
