# Security Review: PyYAML 5.3

The codebase is PyYAML 5.3, the Python YAML parser/serializer. The library implements a multi-loader architecture where different Loaders support different YAML tag types. The security model relies on the loader class hierarchy to separate safe from unsafe deserialization.

## CRITICAL

### `Loader`/`UnsafeLoader` allows arbitrary code execution via `!!python/object/apply:` tag
- **File:** `constructor.py:625` — `instance = self.make_python_instance(suffix, node, args, kwds, newobj)`
- **Evidence:**
  ```
  instance = self.make_python_instance(suffix, node, args, kwds, newobj)
  ```
  The `!!python/object/apply:` multi-constructor is registered on `UnsafeConstructor` at `constructor.py:714-716`. `UnsafeConstructor` is the constructor for both `Loader` and `UnsafeLoader` (lines 41-49, 55-62 of loader.py). The `make_python_instance` method at line 575 resolves the attacker-controlled `suffix` to any callable via `find_python_name` (line 545: `getattr(module, object_name)`) then invokes it with attacker-controlled `args` and `kwds` as `cls(*args, **kwds)`.
- **Attack Tree:**
  ```
  yaml.load(attacker_yaml, Loader=yaml.Loader) — entry: YAML string from attacker
    └─ constructor.py:71 — Loader(stream) constructs FullConstructor → Constructor (UnsafeConstructor alias)
      └─ constructor.py:69-78 — multi-constructor lookup matches !!python/object/apply: prefix
        └─ constructor.py:618-624 — parse attacker YAML mapping into args, kwds, state dicts
          └─ constructor.py:625 — make_python_instance(suffix=attacker, args=attacker, kwds=attacker)
            └─ constructor.py:545 — find_python_name("os.system") → getattr(module, "system")
              └─ constructor.py:575 — cls(*args, **kwds) → os.system("id") — RCE
  ```
- **Impact:** Attacker achieves arbitrary Python code execution. `os.system("id")`, `__import__("os").popen("...")`, or any callable with any arguments. The suffix resolves from any module in `sys.modules` (or imports it via `__import__` in `unsafe=True` mode on lines 513/533).
- **Exploit:**
  ```yaml
  !!python/object/apply:os.system
  args: ["id"]
  ```
  or short form:
  ```yaml
  !!python/object/apply:os.system ["id"]
  ```
- **Remediation:** Never use `yaml.load(stream, Loader=yaml.Loader)` or `yaml.load(stream, Loader=yaml.UnsafeLoader)` with untrusted input. These are documented as unsafe. Use `yaml.safe_load()` with `SafeLoader` for untrusted data:
  ```python
  # Instead of:
  yaml.load(stream, Loader=yaml.Loader)  # UNSAFE
  # Use:
  yaml.safe_load(stream)                  # Safe for untrusted input
  ```

## HIGH

### `FullLoader` allows arbitrary class instantiation via `!!python/object/new:` tag
- **File:** `constructor.py:573` — `return cls.__new__(cls, *args, **kwds)`
- **Evidence:**
  ```python
  if newobj and isinstance(cls, type):
      return cls.__new__(cls, *args, **kwds)
  ```
  `FullConstructor` registers `!!python/object/new:` at `constructor.py:698-700`. When parsed, `make_python_instance` resolves the suffix to any class from a previously-imported module (line 567: `find_python_name`), checks `isinstance(cls, type)`, then calls `cls.__new__(cls, *args, **kwds)` with attacker-controlled arguments. For any class whose `__new__` has side effects, this enables code execution. Additionally, `find_python_name` at lines 526-530 defaults `module_name` to `builtins` when no dot is present, allowing access to built-in types.
- **Attack Tree:**
  ```
  yaml.load(attacker_yaml, Loader=yaml.FullLoader) — entry: YAML string from attacker
    └─ loader.py:28 — FullLoader(stream) constructs FullConstructor
      └─ constructor.py:75-77 — multi-constructor matches !!python/object/new: prefix
        └─ constructor.py:635-636 — construct_python_object_new → construct_python_object_apply(newobj=True)
          └─ constructor.py:613 — args = self.construct_sequence(node, deep=True) — attacker-controlled list
            └─ constructor.py:625 — make_python_instance(suffix, node, args, kwds, True)
              └─ constructor.py:567 — find_python_name("subprocess.Popen") → <class 'subprocess.Popen'>
                └─ constructor.py:572-573 — Popen.__new__(Popen, *attacker_args) — type-dependent side effects
  ```
- **Impact:** Attacker can instantiate any class from any already-imported module with attacker-controlled constructor arguments. While standard library class `__new__` implementations typically delegate to `object.__new__` (ignoring extra args), application code that defines classes with custom `__new__` methods containing side effects becomes exploitable. A single `!!python/object/new:module.Dangerous [payload]` where `Dangerous.__new__` calls `os.system()` achieves RCE. This is a restricted deserialization primitive that becomes fully exploitable when any dangerous user-defined class exists in the same process.
- **Exploit (demonstrating the primitive against a hypothetical dangerous class):**
  ```yaml
  !!python/object/new:myapp.config.DangerousClass
  args: ["rm -rf /tmp/target"]
  ```
  Against standard library classes (demonstrating module access, not RCE):
  ```yaml
  !!python/object/new:builtins.int [42]
  ```
- **Remediation:** Remove `!!python/object/new:` registration from `FullConstructor`, or block it for untrusted input. Add a tag denylist to `FullConstructor` that prevents object instantiation tags. Applications processing untrusted YAML should use `yaml.safe_load()` which uses `SafeConstructor` and has no object instantiation tags:
  ```python
  yaml.safe_load(stream)  # SafeLoader has no !!python/object/* or !!python/name/* tags
  ```

## MEDIUM

### `FullLoader` allows state injection via `!!python/object:` tag (`__setstate__` / `__dict__.update`)
- **File:** `constructor.py:598` — `self.set_python_instance_state(instance, state)`
- **Evidence:**
  ```python
  def construct_python_object(self, suffix, node):
      instance = self.make_python_instance(suffix, node, newobj=True)
      yield instance
      deep = hasattr(instance, '__setstate__')
      state = self.construct_mapping(node, deep=deep)
      self.set_python_instance_state(instance, state)
  ```
  `FullConstructor` registers `!!python/object:` at `constructor.py:694-696`. This creates an instance of any class from an already-imported module, then passes a `set_python_instance_state` calls `instance.__setstate__(state)` or `instance.__dict__.update(state)`. For classes with non-trivial `__setstate__` this allows setting arbitrary attributes or triggering logic in `__setstate__`.
- **Attack Tree:**
  ```
  yaml.load(attacker_yaml, Loader=yaml.FullLoader) — entry: YAML string from attacker
    └─ constructor.py:694-696 — !!python/object: registered on FullConstructor
      └─ constructor.py:591-598 — construct_python_object: creates instance from suffix
        └─ constructor.py:594-595 — instance = self.make_python_instance(suffix, node, newobj=True)
          └─ constructor.py:597 — state = self.construct_mapping(node) — attacker-controlled dict
            └─ constructor.py:598 — self.set_python_instance_state(instance, state)
              └─ constructor.py:579 — instance.__setstate__(state) — class-defined side effects
  ```
- **Impact:** Attacker can create instances of any already-imported class with attacker-controlled state. For classes that have `__setstate__` implementations with side effects (e.g., file I/O, network calls, or other mutations), this provides a deserialization attack surface. For classes without `__setstate__`, arbitrary attributes are set on the instance, which can affect application logic when the instance is later used. Exploitation requires a dangerous class to already be imported in the process.
- **Remediation:** Remove `!!python/object:` registration from `FullConstructor`, or add a blocklist that prevents object instantiation tags. Use `yaml.safe_load()` for untrusted input.

### FullLoader allows `!!python/name:` tag returning arbitrary function references from imported modules
- **File:** `constructor.py:545` — `return getattr(module, object_name)`
- **Evidence:**
  ```python
  def find_python_name(self, name, mark, unsafe=False):
      ...
      return getattr(module, object_name)
  ```
  `FullConstructor` registers `!!python/name:` at `constructor.py:686-688`. This resolves `module.name` to `getattr(sys.modules[module], name)` for any module already loaded in `sys.modules`. The resulting Python object (function, class, module, etc.) is returned as a deserialized value and becomes part of the load output.
- **Attack Tree:**
  ```
  yaml.load(attacker_yaml, Loader=yaml.FullLoader) — entry: YAML string from attacker
    └─ constructor.py:686-688 — !!python/name: registered on FullConstructor
      └─ constructor.py:547-552 — construct_python_name(suffix=attacker)
        └─ constructor.py:540-545 — find_python_name("os.system") → function reference
  ```
- **Impact:** Attacker can obtain function references to any name in any already-imported module. While PyYAML itself does not invoke these references, if the application uses the deserialized output in ways that expect "plain data" (e.g., serialization, evaluation, or dynamic dispatch), embedded function references may cause unexpected behavior. On their own, these references do not execute code. This is an information disclosure and data contamination issue when `FullLoader` is used with untrusted input.
- **Remediation:** Remove `!!python/name:` registration from `FullConstructor`. Use `yaml.safe_load()` for untrusted input, which only resolves basic YAML tags (strings, integers, floats, booleans, null, lists, dicts).

## LOW / INFORMATIONAL

### `unsafe_load()` and `UnsafeLoader` are explicitly unsafe aliases
- **File:** `__init__.py:174-192` — `unsafe_load()`, `unsafe_load_all()` functions
- **Evidence:**
  ```python
  def unsafe_load(stream):
      return load(stream, UnsafeLoader)
  ```
  These functions are explicitly documented as unsafe ("Resolve all tags, even those known to be unsafe on untrusted input"). They are convenience aliases for `load(stream, Loader=Loader)`. The naming correctly signals the danger. No implementation fix needed; documentation already reflects the risk.

## Checked and Cleared

- `__init__.py:154-172` — `safe_load()` / `safe_load_all()` use `SafeLoader` with `SafeConstructor` which only resolves basic YAML tags (null, bool, int, float, binary, timestamp, omap, pairs, set, str, seq, map). No object construction tags. Known safe for untrusted input per PyYAML documentation.
- `__init__.py:134-152` — `full_load()` / `full_load_all()` use `FullLoader`. Covered in findings above.
- `reader.py:59-185` — Input reader. Determines encoding, converts to unicode, checks for non-printable characters. No code execution paths.
- `scanner.py` (all) — Lexical scanner. Produces tokens from YAML stream. No evaluation or code execution.
- `parser.py` — Recursive descent parser. Produces parse events. No code execution.
- `composer.py` — Compose parse events into representation tree. No code execution.
- `resolver.py:143-166` — Tag resolution via regex matching and path-based rules. No code execution.
- `emitter.py` — Outputs YAML events. No input handling.
- `serializer.py:27-110` — Serializes representation tree to YAML stream. Output only.
- `representer.py` — Converts Python objects to representation nodes. Used for dumping (output), not loading (input).
- `dumper.py` — `BaseDumper`, `SafeDumper`, `Dumper` — output-only pipelines.
- `constructor.py:163-472` — `SafeConstructor` — only resolves basic YAML tags. Checked and safe per PyYAML documentation. No `!!python/object/apply:`, `!!python/object/new:`, `!!python/object:`, `!!python/name:`, or `!!python/module:` tags registered.
- `constructor.py:476-506` — `FullConstructor` basic type constructors (python/str, python/unicode, python/bytes, python/int, python/long, python/float, python/complex, python/list, python/tuple, python/dict). These only parse data, not execute code.
- `cyaml.py` — C extension wrappers. Same constructor/dumper classes as Python equivalents.

## Dependencies

No package manifest (setup.py, pyproject.toml, requirements.txt) was found in the review scope. PyYAML is reviewed as a standalone library. No vulnerable dependencies detected.

## Remediation Summary

### Immediate (CRITICAL)
1. `constructor.py:714-716` — Remove `!!python/object/apply:` registration from `UnsafeConstructor` or ensure `Loader`/`UnsafeLoader` are never used with untrusted input. Always use `yaml.safe_load()`.

### Short-term (HIGH/MEDIUM)
2. `constructor.py:698-700` — Remove `!!python/object/new:` registration from `FullConstructor` or add a runtime toggle to disable object instantiation tags for untrusted data paths.
3. `constructor.py:694-696` — Remove `!!python/object:` registration from `FullConstructor` or disable for untrusted data paths.
4. `constructor.py:686-688` — Remove `!!python/name:` and `!!python/module:` registrations from `FullConstructor` for stricter data-only parsing.

### Hardening (LOW)
5. FullLoader is not "safe" — consider renaming or adding explicit warnings in documentation to clarify that `yaml.load(stream)` without `Loader=` and even with `Loader=FullLoader` is NOT equivalent to `yaml.safe_load(stream)` for untrusted input.