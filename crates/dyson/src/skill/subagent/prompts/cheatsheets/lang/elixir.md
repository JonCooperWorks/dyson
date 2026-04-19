Starting points for Elixir — not exhaustive. Erlang-VM model constrains surface but the dynamic layer has sharp edges. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `System.cmd(user, args)` — `user` as the binary is RCE.  Wrapping via `sh -c` via `System.cmd("sh", ["-c", user])` is full shell RCE.
- `:os.cmd(user_charlist)` (Erlang) — shell-expanded string; RCE.
- `Port.open({:spawn, user_str}, _)` — shell-expansion path.

**Dynamic code / atom injection**
- `Code.eval_string(user_str)` — arbitrary Elixir eval.
- `String.to_atom(user_str)` — unbounded atom creation is a DoS (atoms are not GC'd, default limit ~1M).  Use `String.to_existing_atom/1` on attacker input.
- `apply(module, user_fn, args)` — dynamic dispatch; `user_fn` as attacker atom calls any exported function.
- `:erlang.apply(m, f, a)` — same.

**Deserialization**
- `:erlang.binary_to_term(user_bytes)` — RCE via crafted BERT terms that construct atoms, funs, or other dangerous values.  Always use `[:safe]` option: `binary_to_term(bytes, [:safe])`.
- `Plug.Crypto.non_executable_binary_to_term/2` — the correct safe wrapper in Phoenix ecosystems.

**SQL / Ecto**
- `Ecto.Adapters.SQL.query!(repo, "... #{user}", [])` — interpolation in the query string.
- `Ecto.Query.API.fragment("... #{user}")` — dynamic fragments with interpolation.  Use `fragment("?", ^user)` pin form.
- Raw `Ecto.Adapters.SQL.query(repo, user_sql, params)` with user-constructed SQL.

**Path / file**
- `File.read!(user_path)`, `File.write!(user_path, data)` — traversal unless anchored.  `Path.safe_relative_to/2` helps.
- `File.open!(user_path)` with attacker path.
- `:code.priv_dir(user_app)` where `user_app` is attacker-controlled atom.

**Template / XSS**
- EEx: `EEx.eval_string(user_template, ...)` — SSTI.
- Phoenix `raw/1` bypass of HTML escaping — user input inside `raw(user_string)` is XSS.
- Leex / HEEx templates: `{{= user}}` (Elixir) vs `{{user}}` (escaped) conventions — check the template engine version.

**Atom exhaustion (DoS; borderline in-scope)**
- Normally DoS is out-of-scope, but atom exhaustion can escalate to VM crash which takes down the entire BEAM node.  `String.to_atom` on user data with no cap is a finding if the blast radius spans multiple tenants.

**Crypto**
- `:crypto.hash(:md5, ...)` / `:sha` for password hashing — use `:crypto.pbkdf2_hmac` or a library (argon2_elixir).
- `:crypto.strong_rand_bytes/1` is correct; `:rand.uniform` is NOT cryptographic.
- `==` on MAC comparison — use `Plug.Crypto.secure_compare/2`.

## Tree-sitter seeds (elixir)

```scheme
; System.cmd / :os.cmd
(call target: (dot left: (alias) @mod right: (identifier) @fn)
  (#eq? @mod "System")
  (#eq? @fn "cmd"))

; Code.eval_string / String.to_atom / apply
(call target: (dot left: (alias) @mod right: (identifier) @fn)
  (#match? @mod "^(Code|String|Enum)$")
  (#match? @fn "^(eval_string|to_atom|apply)$"))

; binary_to_term without [:safe]
(call target: (dot left: (alias) @mod right: (identifier) @fn)
  (#eq? @mod ":erlang")
  (#eq? @fn "binary_to_term"))
```
