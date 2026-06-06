Starting points for Ruby — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- Backticks: `` `command #{user}` `` — shell-expanded, unsafe.
- `system(user_str)`, `exec(user_str)`, `%x{cmd #{user}}`, `Kernel.spawn` — RCE when user-controlled.
- `Open3.popen3(cmd_str)` with a single string arg — shell-expanded; array form `Open3.popen3(bin, arg1, arg2)` is safe.
- `IO.popen("| cmd #{user}")` — pipe syntax is a shell invocation.

**Eval / dynamic code**
- `eval(user_str)`, `instance_eval(user_str)`, `class_eval(user_str)`, `module_eval`.
- `binding.eval`, `Kernel.eval`.
- `Object.const_get(user_str)` — class lookup by string; can land on any loaded constant.

**Reflection (Ruby's prototype-walk primitive)**
- `obj.send(user_method_name, *args)` — invokes ANY method including private `__send__`, `system`, `eval`.
- `obj.public_send(user_name)` — guards against privates but still attacker-picked method.
- `Object.const_get(user_name).new(*args)` — class-from-string + instantiation.
- `Marshal.load(user_bytes)` — RCE on untrusted bytes (serialized ruby objects).
- `YAML.load(user_str)` (pre-Ruby 3.1 default) — RCE via object deserialization.  `YAML.safe_load` is the safe alternative.
- `JSON.load` with `create_additions: true` (default in older stdlib) — similar to `Marshal.load` — RCE.  Use `JSON.parse`.

**SQL injection**
- `User.where("name = '#{params[:name]}'")` — string interpolation in ORM scope.
- `Model.find_by_sql("... #{user}")`, `connection.execute("... #{user}")`, `exec_query` with interpolation.
- ActiveRecord `order(params[:sort])`, `group(params[:g])` — column names unsanitized → SQLi via injected SQL fragments.

**Path / file**
- `File.read(user_path)`, `File.open(user_path)`, `IO.read(user_path)` — traversal unless `File.expand_path` anchored to a base.
- `File.open(user_path, "w")`, `File.write` — attacker-chosen destination.
- `Dir.glob(user_pattern)` — glob injection; `**/**/*` patterns can DoS or exfiltrate.

**Template injection**
- ERB: `ERB.new(user_template).result(binding)` — SSTI (arbitrary Ruby).
- Liquid / Slim / HAML: pass user input only as data, not as a template source.
- `raw(user_html)`, `html_safe`, `sanitize(user, tags:)` with attacker-supplied allowlist.

**Crypto / randomness**
- `SecureRandom` is fine; `Random.new(seed)` with a predictable seed is not.
- `OpenSSL::HMAC.hexdigest` then string-compare via `==` is timing-unsafe — use `ActiveSupport::SecurityUtils.secure_compare`.

## Tree-sitter seeds (ruby)

```scheme
; eval / instance_eval / class_eval / module_eval
(call method: (identifier) @m
  (#match? @m "^(eval|instance_eval|class_eval|module_eval)$"))

; send / public_send — reflection
(call method: (identifier) @m
  (#match? @m "^(send|public_send|__send__)$"))

; Marshal.load / YAML.load / JSON.load
(call
  receiver: (constant) @c
  method: (identifier) @m
  (#match? @c "^(Marshal|YAML|JSON)$")
  (#eq? @m "load"))

; system / exec with string argument
(call method: (identifier) @m
  (#match? @m "^(system|exec|spawn)$"))

; backtick command
(command_call) @cmd
```
