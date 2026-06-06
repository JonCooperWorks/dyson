Starting points for Erlang — not exhaustive. BEAM-level constraints close many attacks; dynamic-atom and dynamic-dispatch primitives reopen them. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `os:cmd(UserCharlist)` — shell-expanded; RCE.
- `erlang:open_port({spawn, UserStr}, Opts)` — shell-expansion path.
- `erlang:open_port({spawn_executable, UserBin}, Opts)` — attacker-chosen binary = RCE.

**Dynamic dispatch (Erlang's prototype-walk primitive)**
- `apply(Mod, Fn, Args)` with attacker-controlled `Mod` / `Fn` — arbitrary exported function invocation.
- `Mod:Fn(Args)` where `Mod` or `Fn` come from a binary-to-atom-converted user string — same.
- `erlang:list_to_atom(UserStr)` / `binary_to_atom(UserBin, utf8)` — unbounded atom creation is DoS (atoms are not GC'd; default cap ~1M).  Use `list_to_existing_atom` / `binary_to_existing_atom`.

**Deserialization**
- `binary_to_term(UserBin)` — CRITICAL: constructs arbitrary terms including atoms, funs, and references.  Always pass `[safe]` option: `binary_to_term(Bin, [safe])`.
- `erlang:binary_to_term/1` absence of `[safe]` is the #1 Erlang RCE pattern.

**Code loading**
- `code:load_file(UserMod)` — loads a compiled module from disk by name.  Attacker-controlled module name + writable code path = RCE.
- `code:load_binary(UserMod, Filename, UserBin)` — loads user-supplied bytecode.  RCE.

**Ets / Mnesia**
- `ets:match(UserTable, UserPattern)` — a `ms_transform` match pattern isn't code, but allowing the caller to select the table can leak data across tenants.
- Mnesia raw `mnesia:dirty_read({Table, UserKey})` — no access control at the data layer.

**Cowboy / Elli / Mochiweb (web frameworks)**
- Handler callbacks receive `Req` — `cowboy_req:binding(Key, Req)` for URL params, `cowboy_req:read_body(Req)` for body bytes.  Untyped bytes feeding into `binary_to_term` without `[safe]` = RCE.
- `cowboy_req:reply(Code, Headers, Body, Req)` — `Body` as untrusted HTML = XSS.  Template engines (`erlydtl`) should escape; `safe` directive bypasses.

**Crypto**
- `crypto:strong_rand_bytes/1` is correct; `random:uniform` is NOT cryptographic.
- `crypto:hash(md5, _)` / `sha` for password hashing — use `crypto:pbkdf2_hmac` with many iterations, or a library.
- `==` on MACs — timing-unsafe; `crypto:exor` + iterate, or use a constant-time helper.

**File / path**
- `file:read_file(UserPath)`, `file:open(UserPath, _)` — traversal unless anchored.
- `file:list_dir(UserPath)` — directory enumeration.

**Supervision-tree quirks**
- `supervisor:start_child` with user-supplied MFA (Module, Function, Args) — the process tree obeys whatever you pass; attacker-controlled MFA is RCE.
- `gen_server:call/cast` to a named server with user-controlled message — DoS / logic depending on server code.

## Tree-sitter seeds (erlang)

```scheme
; apply(Mod, Fn, Args) / direct Mod:Fn(...)
(function_call
  expr: (atom) @mod
  args: (arguments))

(remote expr1: (atom) @mod expr2: (atom) @fn)

; binary_to_term / list_to_atom family
(call
  expr: (remote expr1: (atom) @mod expr2: (atom) @fn)
  (#match? @mod "^(erlang|binary|crypto|code|os)$")
  (#match? @fn "^(binary_to_term|list_to_atom|binary_to_atom|load_file|load_binary|cmd|open_port)$"))
```
