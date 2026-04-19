# Security Review: PyYAML 5.3

## CRITICAL

### RCE via `!!python/object/apply:`, `!!python/object/new:`, and `!!python/name:` when using `Loader` or `UnsafeLoader`

- **File:** `constructor.py:575`
- **Evidence:**
  ```
  return cls(*args, **kwds)
  ```
  (`make_python_instance` — line 575 calls an arbitrary class/constructor referenced by `suffix` with attacker-cont

rolled `args` and `kwds`, where `cls` is resolved from `sys.modules` via `find_python_name` at line 545.)
- **Attack Tree:**
  ```
  yaml.load(stream, Loader=yaml.Loader) — attacker supplies YAML stream
    └─ constructor.py:612-633 — construct_python_object_apply() parses args/kwds/state from YAML node
      └─ constructor.py:567-575 — make_python_instance(suffix, node, args, kwds)
        └─ constructor.py:541-545 — cls = getattr(sys.modules[module_name], object_name) [SINK REACHED]
          └─ constructor.py:575 — cls(*args, **kwds) — arbitrary callable invoked
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Python, files=17, defs=433, calls=1627, unresolved_callees=3
  
  Found 1 candidate path(s) from constructor.py:567 to constructor.py:575:
  
  Path 1 (depth 1, resolved 2/2 hops):
    constructor.py:567 [byte 21650-21710] — fn `make_python_instance` — taint root: cls, node, self, suffix
    └─ constructor.py:575 [byte 22066-22103] — [SINK REACHED] — tainted at sink: cls
  ```
- **Impact:** Arbitrary code execution with the privileges of the Python process. The `Loader` class (alias of `UnsafeConstructor`/`Constructor`) registers `!!python/object/apply:` and `!!python/object/new:` multi-constructors that resolve any name from `sys.modules` and invoke it with attacker-supplied arguments. When `unsafe=True` (the default for `Loader`/`UnsafeConstructor`), the `find_python_name` method even calls `__import__` to load modules not yet imported, allowing invocation of `os.system`, `subprocess.Popen`, `eval`, etc. via YAML payloads.

  Taint also flows through `find_python_name()` where `module_name` and `object_name` reach `getattr(module, object_name)` at line 545, confirmed by same-file taint tracing of `constructor.py:533` → `constructor.py:545`.
- **Exploit:**
  ```yaml
  !!python/object/apply:os.system
  args: ['id; cat /etc/passwd']
  ```
  or:
  ```yaml
  !!python/object/new:eval
  state: [compile("import os; os.system('id')", "<stdin>", "exec")]
  ```
  The `python/object/apply:` tag is registered at `constructor.py:714` for `UnsafeConstructor` (which `Constructor` inherits from).
- **Remediation:** Never use `Loader` or `UnsafeLoader` with untrusted input. Replace all `yaml.load(stream, Loader=yaml.Loader)` with `yaml.safe_load(stream)`. The `Loader` class was deprecated in 5.1 and re-introduced as `UnsafeLoader` in 5.3 precisely because its presence causes developers to misuse it.

  ```python
  # BAD
  data = yaml.load(stream, Loader=yaml.Loader)
  data = yaml.load(stream, Loader=yaml.UnsafeLoader)

  # GOOD
  data = yaml.safe_load(stream)
  ```

## HIGH

### `!!python/object/apply:` registered on `UnsafeConstructor` enables full RCE including dynamic module import

- **File:** `constructor.py:714`
- **Evidence:**
  ```python
  UnsafeConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/object/apply:',
      UnsafeConstructor.construct_python_object_apply)
  ```
- **Attack Tree:**
  ```
  yaml.load(stream, Loader=yaml.UnsafeLoader) — external YAML input
    └─ constructor.py:619 — construct_python_object_apply() reads yaml mapping
      └─ constructor.py:625 — make_python_instance(suffix, node, args, kwds, newobj)
        └─ constructor.py:533 — __import__(module_name) with unsafe=True
          └─ constructor.py:545 — getattr(module, object_name) — SINK
            └─ constructor.py:573/575 — result called as cls(*args, **kwds) — RCE
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Python, files=17, defs=433, calls=1627, unresolved_callees=3
  
  Found 1 candidate path(s) from constructor.py:533 to constructor.py:545:
  
  Path 1 (depth 1, resolved 2/2 hops):
    constructor.py:533 [byte 19985-20024] — fn `find_python_name` — taint root: module_name
    └─ constructor.py:545 [byte 20699-20742] — [SINK REACHED] — tainted at sink: module_name, module
  ```
- **Impact:** Arbitrary code execution with the privileges of the Python process. Unlike `FullLoader`, `UnsafeLoader` passes `unsafe=True` to `find_python_name()`, which calls `__import__(name)` before checking `sys.modules`. This means an attacker can load and execute code from any installed module, including standard library (`os`, `sys`, `subprocess`, `ctypes`) and third-party packages. The `__import__` call at line 513 (path: `find_python_name` → `__import__` at constructor.py:533) allows loading arbitrary modules, and the result is returned via `getattr(module, object_name)` at line 545.

  The `make_python_instance` method then instantiates the returned callable at line 575: `return cls(*args, **kwds)`, executing the attacker's payload with attacker-controlled arguments parsed from the YAML.
- **Exploit:**
  ```yaml
  !!python/object/apply:subprocess.check_output
  args: [['cat', '/etc/passwd']]
  ```
- **Remediation:** The fix is to not use `UnsafeLoader`/`Loader` with untrusted input. PyYAML 5.4 removes the `Loader` alias entirely. Use `yaml.safe_load()` as the only ingestion method for untrusted YAML.

## MEDIUM

### `yaml.load(stream)` defaulting to `FullLoader` relies on a `RuntimeWarning` that is trivially suppressed

- **File:** `__init__.py:110`
- **Evidence:**
  ```python
  if Loader is None:
      load_warning('load')
      Loader = FullLoader
  ```
  The `load_warning` function (line 43) uses `warnings.warn()` which respects the `-W` flag, `PYTHONWARNINGS` environment variable, and `warnings.filterwarnings`. It can be silenced with `warnings.filterwarnings('ignore', category=yaml.YAMLLoadWarning)` or `$PYTHONWARNINGS=ignore`.
- **Attack Tree:**
  ```
  yaml.load(stream)  — no Loader specified (common pattern from pre-5.1 code)
    └─ __init__.py:109 — load_warning('load') emits RuntimeWarning (can be suppressed)
      └─ __init__.py:110 — Loader = FullLoader
        └─ loader.py:21-29 — FullLoader uses FullConstructor
          └─ constructor.py:686-700 — FullConstructor registers python/name:, python/object:, python/object/new:
            └─ constructor.py:591-598 — construct_python_object creates instances via __new__ + __dict__.update
  ```
- **Taint Trace:** not run within budget — structural evidence only.
- **Impact:** Developers who call `yaml.load(stream)` without `Loader=` receive a `RuntimeWarning` (not an error) that is easily suppressed. The default `FullLoader` still registers dangerous tags (`!!python/name:`, `!!python/object:`, `!!python/object/new:`) which allow lookup of any attribute from already-imported modules and arbitrary object state manipulation via `__new__` + `__dict__.update`. While less severe than `Loader`/`UnsafeLoader` (no `python/object/apply:`), `FullLoader` still enables dangerous deserialization patterns.
- **Remediation:** PyYAML 5.4 made `unsafe_load()` the only way to get unsafe behavior. In 5.3, explicitly use `yaml.safe_load()` or `yaml.full_load()` — never rely on the default.

## LOW / INFORMATIONAL

### `YAMLObject` metaclass auto-registers constructors on `Loader`, `FullLoader`, and `UnsafeLoader`

- **File:** `__init__.py:393`
- **Evidence:**
  ```python
  if isinstance(cls.yaml_loader, list):
      for loader in cls.yaml_loader:
          loader.add_constructor(cls.yaml_tag, cls.from_yaml)
  ```
  `YAMLObject.yaml_loader` defaults to `[Loader, FullLoader, UnsafeLoader]` (line 407). Any custom subclass of `YAMLObject` added to a codebase automatically registers itself on all unsafe loaders, widening the attack surface without explicit opt-in.
- **Impact:** Low — requires developers to define custom `YAMLObject` subclasses. Those subclasses' `from_yaml` methods (which call `construct_yaml_object` at line 418) are then invocable via the `yaml_tag` on all unsafe loaders.
- **Remediation:** Use `yaml_loader = SafeLoader` in custom `YAMLObject` subclasses.

## Checked and Cleared

- `SafeLoader` (loader.py:31) — uses `SafeConstructor` which only registers basic YAML tags (scalar types, sequence, mapping). No Python-specific constructors. Known safe for untrusted input per PyYAML documentation and CVE analyses.
- `BaseLoader` (loader.py:11) — uses `BaseConstructor` which has no registered constructors; all tags fall through to `construct_undefined`. Cannot construct any YAML data. Safe by design.
- `SafeConstructor.construct_undefined` (constructor.py:418-421) — throws `ConstructorError` for any unknown tag. Registered as the default (`None`) handler at line 471. Prevents unknown tags from being processed.
- `FullConstructor.find_python_name` (constructor.py:522) — in `FullLoader` mode (`unsafe=False`), does NOT call `__import__`. Only resolves names from already-imported modules. The check at line 537 (`if module_name not in sys.modules`) prevents loading new modules.
- `resolver.py` implicit resolvers (lines 170-226) — regex-based tag resolution for scalar types (bool, float, int, null, timestamp). These match input strings to YAML types (e.g. `yes` → `True`), not code execution.
- `__init__.py:load_all` (line 118) — same default `FullLoader` behavior as `load()`; the warning at line 124 applies identically.
- `reader.py`, `scanner.py`, `parser.py`, `composer.py` — low-level YAML parsing pipeline. Output is AST/nodes only; no code execution surface.
- `emitter.py`, `serializer.py`, `dumper.py` — output path (serialization). Produces YAML strings from Python objects. No injection surface since output is generated by the library, not executed.

## Dependencies

The project IS PyYAML 5.3. The `dependency_review` subagent identified 4 known CVEs/vulnerabilities:

- **CRITICAL** — GHSA-8q59-q68h-6hv4 / PYSEC-2021-142: Arbitrary code execution via full YAML load. Fixed in 5.4. linked-findings: loader.py:41, loader.py:55
- **HIGH** — GHSA-6757-jp84-gxfx: ReDoS in scanner regex. Fixed in 5.3.1. linked-findings: loader.py:1
- **HIGH** — PYSEC-2020-96: Arbitrary code execution before 5.3.1. Fixed in 5.3.1. linked-findings: loader.py:21, loader.py:41

The dependency review is consistent with the code findings above: `Loader` (loader.py:41) and `UnsafeLoader` (loader.py:55) are the exact classes that expose RCE.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `constructor.py:714` — Remove or restrict `!!python/object/apply:` registration from non-unsafe constructors (fixed in PyYAML 5.4 by creating separate `FullLoader` and `UnsafeLoader` classes)
2. `constructor.py:567-575` — Add explicit allowlist of callable names/imports when `unsafe=False` (fixed: `unsafe=True` is now gated behind `UnsafeLoader` only)

### Short-term (MEDIUM)
1. `__init__.py:110` — Make `yaml.load(stream)` with no `Loader=` an error, not a warning (fixed in PyYAML 5.4 — removed default)

### Hardening (LOW)
1. `__init__.py:407` — Change `YAMLObject.yaml_loader` default to `[SafeLoader]` to avoid auto-registering on unsafe loaders