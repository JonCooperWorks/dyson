Starting points for Haskell — not exhaustive. Strong types close many classes, but FFI + `unsafe*` + effectful monads reintroduce the usual risks. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `System.Process.callCommand user` / `readCreateProcess (shell user) ""` — shell-expanded, RCE with user input.
- `System.Process.createProcess (proc user [])` — `user` as the program path is RCE; `proc` list form avoids shell but still RCE if binary is attacker-controlled.
- `System.Posix.Process.executeFile user False args env` — same.

**Unsafe exits from the type system**
- `unsafePerformIO` called with untrusted effects — side-effectful IO in a "pure" context can break invariants upstream.
- `unsafeCoerce :: a -> b` — raw type-punning.  With network bytes, same unsoundness as Rust `transmute`.
- FFI: `Foreign.Marshal.Alloc.malloc` / `alloca` with a user-controlled size; `peekArray n ptr` where `n` is user-supplied → OOB.

**Dynamic dispatch / reflection-ish**
- Haskell's `Data.Dynamic`, `Data.Typeable` — not a common attack surface but `fromDynamic` on untrusted `Dynamic` values breaks the "trust" of a typed value.
- Template Haskell `$(runIO ...)` with user-derived code — SSTI-equivalent at compile time; for runtime, `hint` / `plugins` interpreters evaluating user strings = RCE.

**Deserialization**
- `Data.Binary.decode` / `Data.Serialize.decode` on untrusted bytes — type-parameterised but vulnerable to `Int`-size-mismatch bugs, unbounded list lengths (DoS), and `Char` deserialization with values outside `[0, 0x10FFFF]` producing invalid runtime values.
- `aeson` `decode :: FromJSON a => ByteString -> Maybe a` — safe IF the target type is fixed.  `Value`-typed parsing + later walks is the Haskell equivalent of JS's wire-format prototype walk.
- `store` / `flat` / `cereal` with untrusted bytes — same.

**SQL injection**
- `postgresql-simple`: `query_ conn (Query (BS.pack ("SELECT ... " ++ show user)))` — interpolated query string, SQLi.  Use the `Only user` parameter tuple with `?` placeholders.
- `persistent` raw SQL: `rawSql userQuery params` with user-constructed query.

**Path / file**
- `readFile user`, `writeFile user _`, `hGetContents` — traversal.  `System.Directory.makeAbsolute` alone doesn't anchor; combine with a prefix check.

**Web / template**
- Servant handlers taking `String` / `Text` from URL path segments — trust only after validating.
- `Hamlet` / `Lucid` auto-escape HTML; `preEscapedToHtml user` / `toHtmlRaw user` bypass the escape.
- Yesod `mkYesod` routes + `PersistEntity` forms inherit the XSS / SQLi surface of the backing libs.

**Concurrency**
- `MVar` with no strict ordering discipline across threads — deadlock (DoS) but also logic-level race conditions where a sensitive check and an action are interleaved.
- `IORef` without atomics on multi-core — torn reads.

**Crypto**
- `cryptonite` is fine; `System.Random` is NOT cryptographic.  Use `cryptonite`'s `Crypto.Random`.
- `Crypto.Hash.MD5` / `Crypto.Hash.SHA1` for password hashing — use `scrypt` / `bcrypt` / `argon2`.
- `Eq`-based comparison of MACs — timing-unsafe; `Data.ByteArray.eq` is constant-time.

## Tree-sitter seeds (haskell)

```scheme
; callCommand / readCreateProcess / createProcess
(function
  (variable) @f
  (#match? @f "^(callCommand|readCreateProcess|createProcess|executeFile)$"))

; unsafe escapes
(variable) @v (#match? @v "^(unsafePerformIO|unsafeCoerce|unsafeInterleaveIO)$")
```

Haskell's tree-sitter grammar is less uniform than imperative-language grammars — `ast_describe` on a real snippet before writing anything structural.
