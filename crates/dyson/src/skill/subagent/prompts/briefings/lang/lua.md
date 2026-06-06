Starting points for Lua — not exhaustive. Dynamic, embedded; often running inside nginx (OpenResty), game engines, or as scripting in larger systems. Novel sinks outside this list are still in scope.

## Sinks

**Eval / dynamic code**
- `loadstring(user_str)()` (Lua 5.1) / `load(user_str)()` (5.2+) — compiles + runs arbitrary Lua.  RCE with user input.
- `dofile(user_path)` — executes the file as Lua code.
- `loadfile(user_path)()` — same.
- `require(user_module)` — loads a module by name; dynamic require from user input can load attacker-chosen code paths off `package.path`.

**Reflection / metatables**
- `getmetatable(x).__index = user_table` — metatable injection.  If attacker controls `user_table`, later lookups on `x` can land on attacker-chosen `__index` functions = indirect code execution.
- `rawset(t, user_key, user_val)` — bypasses metamethods; usually safer than `t[user_key] = user_val` but removes any enforced guards.
- `_G[user_name]` / `_ENV[user_name]` — walks the global environment by attacker-chosen name; landing on `os.execute`, `io.open`, `loadstring` is the Lua prototype-walk primitive equivalent.

**Command execution**
- `os.execute(user_str)` — shell-expanded; RCE.
- `io.popen(user_str, "r")` — pipe to a shell command; same.
- In OpenResty: `os.execute` is usually disabled/unavailable; but `ngx.pipe.spawn` from lua-resty-shell is available.

**File / path**
- `io.open(user_path, "w")` — traversal + arbitrary write.
- `io.open(user_path, "r")` — traversal + arbitrary read.
- `os.remove(user_path)` — attacker-chosen delete.
- `os.rename(old_user, new_user)` — attacker-chosen move.

**SQL (LuaSQL / lua-resty-mysql / pgmoon)**
- `conn:execute(string.format("... %s", user))` / `string.format("... %s", user)` into DB — SQLi; use placeholders.
- `pgmoon` parameterised query: `db:query("... $1", user)` — safe.  String concat is not.

**Deserialization**
- `cjson.decode(user_str)` — safe (pure JSON).  Walks over the decoded table with attacker-chosen keys are prototype-walk analogue via `_G`/`_ENV` confusion if values re-enter `getfenv` / `_G`.
- Binary serialization libs (`Pickle.lua`, `lua-resty-lrucache` custom serializers) — case by case.

**String / tostring coercion**
- `tostring(x)` calls `__tostring` metamethod; an attacker-controlled object with a malicious `__tostring` can execute code at coercion time.
- `string.format("%s", x)` — same; invokes `tostring`.

**Sandbox escapes**
- `setfenv(fn, env)` (5.1) / `_ENV` (5.2+) — sandboxing an untrusted script requires VERY careful env construction.  Missing a single entry like `require` / `load` / `pcall` in the "blocked" list reopens the world.
- `debug` library (`debug.getinfo`, `debug.sethook`, `debug.setupvalue`) — bypasses most sandboxes.  Must be removed from the sandbox env entirely.

**Crypto (LuaCrypto / lua-resty-openssl)**
- `math.random()` is NOT cryptographic — use `lua-resty-random` or OpenSSL bindings for tokens.
- `ngx.md5` / `ngx.sha1` for password hashing — use bcrypt via OpenSSL bindings or scrypt.
- String equality (`==`) on HMACs — timing-unsafe.  Use a constant-time compare.

## Tree-sitter seeds (lua)

Lua's tree-sitter grammar is available via `tree-sitter-lua` (not in dyson's in-tree grammars at time of writing — `ast_query language: "lua"` may fail).  Use `search_files` + `ast_describe` to feel around; for OpenResty-specific reviews, grepping `ngx\.` is often productive.

```scheme
; load / loadstring / loadfile / dofile / require
(function_call
  name: (identifier) @f
  (#match? @f "^(load|loadstring|loadfile|dofile|require)$"))

; os.execute / io.popen / io.open
(function_call
  name: (dot_index_expression
    table: (identifier) @t
    field: (identifier) @fn)
  (#match? @t "^(os|io)$")
  (#match? @fn "^(execute|popen|open|remove|rename)$"))
```
