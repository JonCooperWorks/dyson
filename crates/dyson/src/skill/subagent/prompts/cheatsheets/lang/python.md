Starting points for Python — not exhaustive. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `subprocess.run(cmd, shell=True)`, `subprocess.Popen(cmd, shell=True)`, `subprocess.call(..., shell=True)` — `shell=True` with any string that touches user input is RCE. Even a list argument flows to the shell when `shell=True` is set.
- `os.system(x)`, `os.popen(x)` — always a shell; user input reaching either is RCE.
- `commands.getoutput`, `commands.getstatusoutput` (py2 legacy).

**Eval / dynamic code**
- `eval(x)`, `exec(x)`, `compile(x, ...)`.
- `__import__(user_str)`, `importlib.import_module(user_str)` — loads attacker-named modules; RCE via side-effectful imports.

**Reflection / property walk (RCE primitive)**
- `getattr(obj, user_name)`, `setattr`, `delattr`, `hasattr` with user-supplied attribute name. Landing on dunders (`__globals__`, `__builtins__`, `__class__`, `__subclasses__`, `__mro__`) reaches `eval` / `exec` / arbitrary class instantiation.
- `operator.attrgetter(user_str)(obj)` — same primitive, prettier.
- A loop `value = value[seg]` over `user_str.split('.')` is the Python prototype-walk primitive.

**Deserialization**
- `pickle.loads`, `pickle.load`, `cPickle.loads` — ANY pickle on untrusted input is RCE.
- `dill.loads`, `shelve.open(path)` on an attacker-controlled file — same.
- `marshal.loads` — RCE on untrusted bytes.
- `yaml.load(data)` without `Loader=yaml.SafeLoader` is RCE. Only `yaml.safe_load` is safe.
- `xml.etree.ElementTree.parse`, `xml.dom.minidom.parseString`, `lxml.etree.parse` — XXE / billion-laughs unless `resolve_entities=False` and DTDs are disabled. Prefer `defusedxml`.

**SQL injection**
- `cursor.execute(f"... {user}")`, `cursor.execute("... %s" % user)`, `.execute("..." + user)` — all SQLi.
- Django `Model.objects.extra(where=[user])`, `.raw(f"... {user}")` — see framework sheet.
- SQLAlchemy `session.execute(text(f"... {user}"))` is SQLi; `text("... :p").bindparams(p=user)` is safe.

**Template injection**
- Jinja2 `Template(user).render()` → SSTI (sandbox-escapable).
- Jinja2 `env = Environment(autoescape=False)` applied to HTML rendering → XSS.
- Django `mark_safe(user)`, `|safe` filter → XSS.
- Mako `Template(autoescape=False)` → XSS + SSTI.

**Path / file**
- `open(user_path)`, `os.path.join(base, user)` — traversal unless `user` is `os.path.basename`-stripped AND `os.path.realpath(joined).startswith(os.path.realpath(base) + os.sep)` is enforced.
- `shutil.copy`/`move`/`rmtree` with user paths.

**SSRF**
- `requests.get(user_url)`, `urllib.request.urlopen(user_url)`, `httpx.get(user_url)` without host allowlist. `file://`, `gopher://` support varies — assume worst.

**LDAP / NoSQL**
- `ldap.search_s(base, scope, filter_with_user_str)` — LDAP injection.
- `collection.find({"$where": user_js})` in pymongo → JS injection server-side.

## Tree-sitter seeds (python)

```scheme
; All pickle/marshal/yaml/cPickle .loads / .load calls
(call function: (attribute
    object: (identifier) @mod
    attribute: (identifier) @fn) @c
  (#match? @mod "^(pickle|cPickle|marshal|dill)$")
  (#match? @fn "^(loads?|load)$"))

; eval / exec / compile
(call function: (identifier) @f
  (#match? @f "^(eval|exec|compile)$"))

; subprocess with shell=True (find keyword args, then verify value=True manually)
(call function: (attribute attribute: (identifier) @fn) @c
  (#match? @fn "^(run|Popen|call|check_output|check_call)$"))

; os.system / os.popen
(call function: (attribute
    object: (identifier) @mod
    attribute: (identifier) @fn)
  (#eq? @mod "os")
  (#match? @fn "^(system|popen)$"))

; getattr / setattr — prototype-walk primitive
(call function: (identifier) @f
  (#match? @f "^(getattr|setattr|hasattr|delattr)$"))
```
