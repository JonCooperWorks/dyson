# Security Review: PyYAML 5.3 — FullLoader RCE (CVE-2020-1747)

## CRITICAL

### FullLoader allows arbitrary Python object instantiation via `python/object/new:` tag, enabling Remote Code Execution
- **File:** `constructor.py:698`
- **Evidence:**
  ```python
  FullConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/object/new:',
      FullConstructor.construct_python_object_new)
  ```
  This registration (lines 698-700) adds a multi-constructor to `FullConstructor` — the constructor used by `FullLoader` — that accepts the `!!python/object/new:SUBCLASS` tag. The tag suffix (e.g. `subprocess.Popen`) is resolved to a Python class and instantiated with attacker-controlled arguments.

- **Attack Tree:**
  ```
  attacker supplies YAML with !!python/object/new:subprocess.Popen tag
    └─ FullLoader.get_single_data() → construct_document → construct_object [constructor.py:41-44, 59]
      └─ construct_object dispatches to yaml_multi_constructors[tag_prefix] [constructor.py:74-78]
        └─ construct_python_object_new(suffix="subprocess.Popen", node) [constructor.py:635-636]
          └─ construct_python_object_apply(suffix, node, newobj=True) [constructor.py:636]
            └─ make_python_instance(suffix, node, args, kwds, newobj=True) [constructor.py:625]
              └─ find_python_name("subprocess.Popen", mark) → resolves class [constructor.py:567]
                └─ cls.__new__(cls, *args, **kwds) → subprocess.Popen.__new__ [constructor.py:573]
                  └─ subprocess.Popen.__init__(args) executes system command [python stdlib]
  ```

- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=Python, files=17, defs=433, calls=1627, unresolved_callees=3

  Found 10 candidate path(s) from constructor.py:635 to constructor.py:575:

  Path 1 (depth 4, resolved 5/5 hops):
    constructor.py:635 [byte 24483-24539] — fn `construct_python_object_new` — taint root: node, self, suffix
    └─ constructor.py:636 [byte 24555-24616] — calls `construct_python_object_apply([suffix], [node], newobj)` → params `self`, `suffix`
      └─ constructor.py:625 [byte 24156-24215] — calls `make_python_instance([suffix], node, [args], [kwds], newobj)` → params `args`, `node`, `self` [AMBIGUOUS]
      └─     - constructor.py:561 make_python_instance()
      └─     - constructor.py:710 make_python_instance()
        └─ constructor.py:573 — cls.__new__(cls, *args, **kwds)
  ```

- **Impact:** Attacker provides a YAML payload to any application using `yaml.load(stream, Loader=FullLoader)` (or `yaml.load(stream)` which defaults to `FullLoader` at `__init__.py:110`). The payload is deserialized, `subprocess.Popen` (or any callable class) is instantiated with attacker-controlled `args`/`kwds`, and arbitrary OS commands execute with the privileges of the Python process. This is remote code execution.

- **Exploit:**
  ```yaml
  !!python/object/new:subprocess.Popen
  args: ['id']
  ```
  Or via the `state` key to trigger `__setstate__`:
  ```yaml
  !!python/object/new:os.system
  args: ['id']
  ```

- **Remediation:** Remove the `python/object/new:` multi-constructor from `FullConstructor`. The safe alternative is `SafeLoader`/`SafeConstructor` which does not register any `python/` multi-constructors. In `constructor.py`, delete lines 698-700:
  ```python
  # REMOVE these lines from FullConstructor:
  FullConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/object/new:',
      FullConstructor.construct_python_object_new)
  ```

### FullConstructor also exposes `python/name:` and `python/module:` tags enabling arbitrary object reference
- **File:** `constructor.py:686`
- **Evidence:**
  ```python
  FullConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/name:',
      FullConstructor.construct_python_name)

  FullConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/module:',
      FullConstructor.construct_python_module)

  FullConstructor.add_multi_constructor(
      'tag:yaml.org,2002:python/object:',
      FullConstructor.construct_python_object)
  ```
  These three registrations (lines 686-696) are also present in `FullConstructor` but absent from `SafeConstructor`. The `python/name:` tag resolves arbitrary qualified names (e.g. `os.system`) via `find_python_name` and returns the resolved callable. The `python/object:` tag (line 694-696) instantiates objects via `__new__` and populates their state via `set_python_instance_state` (line 598), which calls `__setstate__` or `__dict__.update` — both of which can trigger side effects on crafted classes.

  While `python/name:` alone returns a reference rather than calling it, and `python/object:` requires the class to have a safe `__new__`, these primitives chain with `python/object/new:` to expand the attack surface. The `find_python_name` function at line 522-545 permits resolution of any name from any module already imported in `sys.modules`, enabling gadget-chain construction from the standard library.

- **Impact:** The `python/object:` constructor alone can instantiate any class and populate its `__dict__` or call `__setstate__`, allowing exploitation of classes with dangerous `__setstate__` methods. Combined with `python/name:`, an attacker can reference any imported callable. While these are secondary to `python/object/new:`, they expand the gadget pool and should be removed from `FullConstructor` to match `SafeConstructor`'s tag set.

- **Remediation:** Remove `python/name:` (line 686), `python/module:` (line 690), and `python/object:` (line 694) from `FullConstructor`. `FullLoader` should be functionally equivalent to `SafeLoader` minus data types like `python/none`, `python/bool`, `python/int`, etc. which map to safe built-in types.

## Checked and Cleared

- `constructor.py:507-520` (`find_python_module`) — Only called when `unsafe=True` (UnsafeConstructor) or module already in `sys.modules`. In `FullConstructor`, the module must be pre-imported, limiting the name pool but still dangerous when chained with `python/object/new:`.
- `constructor.py:522-545` (`find_python_name`) — Same as above. The `unsafe=False` parameter (used by FullConstructor, line 531) requires the module to already be in `sys.modules`, but this is trivially satisfied for standard library modules like `subprocess`, `os`, `socket`.
- `constructor.py:591-598` (`construct_python_object`) — Uses `__new__` + `__setstate__`/`__dict__.update`. Less directly dangerous than `python/object/new:` but still enables gadget chains. Filed above.
- `constructor.py:698-700` (`python/object/new:` in `UnsafeConstructor` via inheritance) — UnsafeConstructor inherits from FullConstructor and adds `python/object/apply:` (line 714), which is even more dangerous (direct `cls(*args, **kwds)` without `__new__` bypass). This is expected behavior for `UnsafeConstructor`/`Loader` — these are documented as unsafe.
- `loader.py:21-29` (`FullLoader` class) — Correctly inherits from `FullConstructor`. The danger is in the constructor's tag map, not the loader class itself.
- `__init__.py:103-116` (`load()` defaulting to `FullLoader`) — This is the default path that makes the vulnerability widely accessible. However, the issue is `FullLoader`'s constructor, not the defaulting behavior — users who explicitly use `SafeLoader` are unaffected.

## Dependencies

No dependency manifests (package.json, Cargo.toml, go.mod, etc.) found in the review scope. The target is PyYAML 5.3 source itself (`lib3/yaml/`). CVE-2020-1747 is the vulnerability under review.

## Remediation Summary

### Immediate (CRITICAL)
1. `constructor.py:698` — Remove `python/object/new:` multi-constructor from `FullConstructor`
2. `constructor.py:694` — Remove `python/object:` multi-constructor from `FullConstructor`
3. `constructor.py:686` — Remove `python/name:` multi-constructor from `FullConstructor`
4. `constructor.py:690` — Remove `python/module:` multi-constructor from `FullConstructor`

### Short-term
No additional changes needed — removing the four constructors above makes `FullConstructor` functionally equivalent to `SafeConstructor` (plus only safe Python type tags like `python/none`, `python/str`, `python/int`, `python/tuple`).

### Hardening
1. `__init__.py:110` — Consider changing `yaml.load()` default from `FullLoader` to `SafeLoader` to protect users who call `load()` without an explicit loader.