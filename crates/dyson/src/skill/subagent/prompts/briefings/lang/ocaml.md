Starting points for OCaml — not exhaustive. Strong static types close structural classes; effectful I/O, FFI, and marshalling reopen them. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `Sys.command user` — shell-expanded; RCE.
- `Unix.system user` / `Unix.open_process_in user` — shell.
- `Unix.execvp bin argv` — `bin` user-controlled = RCE.  Array form avoids shell but not RCE.

**Dynamic code / reflection**
- `Dynlink.loadfile user_path` — loads a compiled `.cmo` / `.cmxs` at runtime.  Attacker-controlled path + writable bytecode = RCE.
- OCaml lacks runtime reflection; the attack surface is narrower than JVM / .NET.

**Deserialization**
- `Marshal.from_string user_str 0` / `Marshal.from_channel ic` — CRITICAL: OCaml's native format is not type-checked across versions / processes.  Unmarshalling attacker bytes produces values claimed to be of the expected type but with arbitrary memory layout, breaking the type system.  Every production OCaml service must validate input format (JSON / protobuf) instead of using `Marshal` on untrusted bytes.
- `Bin_prot` / `Sexplib` — type-parameterised and safer, but still susceptible to DoS via crafted length prefixes.

**SQL**
- `Postgresql.exec conn (Printf.sprintf "SELECT ... %s" user)` — SQLi.  Use `conn#exec ~params:[|user|] "SELECT ... $1"`.
- Caqti: `Caqti_request.find (tup1 string) string "... WHERE x = ?"` is parameterised; constructing the SQL string from user input defeats the safety.

**Path / file**
- `open_in user_path`, `open_out user_path`, `In_channel.read_all user_path` — traversal.
- `Filename.concat base user` — doesn't prevent `..`; anchor with `realpath` + prefix check.

**Opium / Dream / Cohttp (web frameworks)**
- `Dream.query req "name"` / `Dream.form req` / `Dream.body req` — attacker-controlled sources.
- `Dream.html user_str` — raw HTML response; no escape.  Template engines (Eml, Dream's `.eml.html`) escape by default; explicit `{user}!` / `|!` forms bypass.
- `Dream.redirect req user_url` — open redirect.
- Missing `Dream.authenticate_token` middleware on a state-changing route — finding.

**FFI / Ctypes**
- `Ctypes.string_of (ptr char) user_ptr` without a valid nul-terminated memory region = OOB read.
- `Bigarray.Array1.sub a ofs len` with user-supplied `ofs` / `len` outside the base bounds — OOB.

**Crypto**
- `Cryptokit` for hashing / MAC — `Cryptokit.Hash.md5` / `sha1` for password hashing is wrong; use `Argon2` / `PBKDF2`.
- `Cryptokit.Random.string (Cryptokit.Random.pseudo_rng ...) 32` — `pseudo_rng` is NOT cryptographic; use `Cryptokit.Random.secure_rng`.
- `String.equal` / `Bytes.equal` on MACs — timing-unsafe.  Constant-time compare required.

**Exn leakage**
- `Failwith "internal detail: %s" user_path` — exception messages bubble up to `500 Internal Server Error` bodies in many web frameworks.  Internal-path leakage is out of scope per rules, but PII / secret leakage is a finding.

## Tree-sitter seeds (ocaml)

```scheme
; Sys.command / Unix.system / Marshal.from_*
(application_expression
  (value_path
    (module_path (module_name) @mod)
    (value_name) @fn)
  (#match? @mod "^(Sys|Unix|Marshal|Dynlink)$")
  (#match? @fn "^(command|system|open_process|open_process_in|execvp|execv|from_string|from_channel|loadfile)$"))
```

OCaml's grammar has complex module-path rules; `ast_describe` before writing anything structural is mandatory, not optional.
