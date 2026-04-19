# Security Review: lodash

## CRITICAL

### Prototype pollution via `_.set` / `_.update` / `_.zipObjectDeep` — no block on `__proto__` or `constructor.prototype` in property path
- **File:** `lodash.js:3987`
- **Evidence:**
  ```js
  var objValue = nested[key];
  ```
  In `baseSet`, the path segments (from attacker-controlled string or array) are used directly as keys in a property-chain walk. Neither `__proto__` nor `constructor`/`prototype` are blocked. The walk traverses through `Object.prototype.constructor → Object.prototype`, then `Object.prototype["prototype"]` which is `Object.prototype` itself, followed by writing the value onto `Object.prototype`.
- **Attack Tree:**
  ```
  _.set(target, 'constructor.prototype.isAdmin', true) [lodash.js:13674]
    └─ baseSet(object, path, value) [lodash.js:3971-3998]
      └─ path.walk: nested = nested['constructor'] → Object constructor function
        └─ nested = nested['constructor']['prototype'] → Object.prototype
          └─ assignValue(nested, key, newValue) [lodash.js:3995]
            └─ Object['prototype'] = {} then Object['prototype'][leafKey] = value [lodash.js:2559]
              └─ object[leafKey] = value on Object.prototype — all {} polluted
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=JavaScript, files=54, defs=1798, calls=5000, unresolved_callees=21

  Found 1 candidate path(s) from lodash.js:3971 to lodash.js:3996:

  Path 1 (depth 1, resolved 2/2 hops):
    lodash.js:3971 [byte 129287-129342] — fn `baseSet` — taint root: customizer, object, path, value
    └─ lodash.js:3996 [byte 130059-130088] — [SINK REACHED] — tainted at sink: nested, length, key, path, newValue, value, lastIndex, objValue, customizer
  ```
- **Impact:** `_.set({}, 'constructor.prototype.isAdmin', true)` or `_.set({}, '__proto__.isAdmin', true)` sets `Object.prototype.isAdmin = true`. Every plain object in the process gains `isAdmin: true`. An attacker who controls a path argument to `_.set`, `_.setWith`, `_.update`, `_.updateWith`, or `_.zipObjectDeep` can inject arbitrary properties into `Object.prototype`, bypassing authorization checks, poisoning template contexts, and enabling downstream RCE when polluted properties flow into security-sensitive logic.
- **Exploit:**
  ```js
  // Direct poisoning via set
  var _ = require('lodash');
  _.set({}, 'constructor.prototype.isAdmin', true);
  // Now every object in the process:
  ({ isAdmin: true, toString: [Function], valueOf: [Function] ... })
  const req = { user: {} };
  if (req.user.isAdmin) { grantAdminAccess(); } // BYPASSED
  ```
- **Remediation:** Reject path segments matching `__proto__`, `constructor`, or `prototype` in `baseSet`:
  ```js
  function baseSet(object, path, value, customizer) {
    // ...existing path parsing...
    for (var i = 0; i < path.length; i++) {
      var seg = toKey(path[i]);
      if (seg === '__proto__' || seg === 'constructor' || seg === 'prototype') return object;
    }
    // ...rest unchanged
  }
  ```

### Prototype pollution via `_.defaultsDeep` — `constructor` key bypasses `safeGet` guard
- **File:** `lodash.js:3640`
- **Evidence:**
  ```js
  function baseMergeDeep(object, source, key, srcIndex, mergeFunc, customizer, stack) {
    var objValue = safeGet(object, key),
        srcValue = safeGet(source, key),
  ```
  `safeGet` (line 6616) blocks only `'__proto__'`. The `constructor` key passes through. In `defaultsDeep`, `customDefaultsMerge` (line 5587) calls `baseMerge(objValue, srcValue, ...)` where `objValue` is `{}.constructor` (the `Object` constructor function). The recursive merge then walks into `Object.prototype` and writes attacker-controlled properties onto it via `assignMergeValue` at line 3698.
- **Attack Tree:**
  ```
  _.defaultsDeep({}, JSON.parse('{"constructor":{"prototype":{"isAdmin":true}}}')) [lodash.js:12837]
    └─ mergeWith(obj, src, customDefaultsMerge) [lodash.js:12839]
      └─ baseMerge({}, {"constructor":{...}}, customDefaultsMerge) [lodash.js:3602]
        └─ baseMergeDeep({}, src, "constructor", ...) [lodash.js:3609]
          └─ customDefaultsMerge: baseMerge(Object, {"prototype":{...}}) [lodash.js:5587]
            └─ baseMergeDeep(Object, src, "prototype", ...)
              └─ safeGet(Object, "prototype") → Object.prototype [line 3640]
                └─ baseMerge(Object.prototype, {"isAdmin":true}, ...) [line 5587]
                  └─ assignMergeValue(Object.prototype, "isAdmin", true) [line 3698]
                    └─ object["isAdmin"] = true on Object.prototype [line 2559]
                      └─ ALL {} objects gain isAdmin: true
  ```
- **Impact:** `_.defaultsDeep({}, JSON.parse('{"constructor":{"prototype":{"isAdmin":true}}}'))` sets `Object.prototype.isAdmin = true`. Same concrete outcome as the `set` primitive: all plain objects inherit the injected property, enabling authorization bypass. The `__proto__` key IS partially guarded by `safeGet`, but `constructor`/`prototype` walk is unbounded.
- **Exploit:**
  ```js
  var _ = require('lodash');
  _.defaultsDeep({}, JSON.parse('{"constructor":{"prototype":{"isAdmin":true}}}'));
  ({}).isAdmin;  // => true — Object.prototype polluted
  ```
- **Remediation:** Extend the `safeGet` blocklist (line 6616) to include `constructor`, and add a block in `assignMergeValue` (line 2454) and `baseAssignValue` (line 2550) to reject both `constructor` and `prototype`:
  ```js
  function safeGet(object, key) {
    if (key == '__proto__' || key == 'constructor') {
      return;
    }
    return object[key];
  }
  ```

## HIGH

No findings beyond the CRITICAL items above. Both prototype pollution primitives cover the full attack surface of path-based and merge-based property manipulation.

## MEDIUM

No findings.

## LOW / INFORMATIONAL

- `lodash.js:1471` — `Math.random` seeded from current time. `_.random()`, `_.sample()`, `_.shuffle()` use `Math.random()`, which is not cryptographically secure. This is expected for a utility library; users requiring CSPRNG should use `crypto.randomBytes`. Not a vulnerability.

## Checked and Cleared

- `lodash.js:2551` — `baseAssignValue` blocks `__proto__` via `Object.defineProperty` (when `defineProperty` exists). Correctly mitigates the most common `__proto__` injection vector in merge-family functions. `__proto__` key alone cannot pollute in `merge`/`defaultsDeep`.
- `lodash.js:6616` — `safeGet` blocks `__proto__` key reads in merge-family functions. Mitigates `__proto__` as a property key in `baseMergeDeep`. Does NOT block `constructor`.
- `lodash.js:6331` — `isKeyable` blocks `__proto__` from being used as a hash cache key. Defense for internal caching, not a user-facing vector.
- `lodash.js:3602` — `baseMerge` uses `keysIn` to iterate source (includes inherited), but combined with `safeGet` at line 3640, `__proto__` as a direct key is blocked. `constructor` is not.
- `lodash.js:14771` — `_.template()` compiles template strings into `Function()`. RCE when the template string itself is attacker-controlled. This is the documented behavior of the function, not a vulnerability within lodash. The known CVE (GHSA-r5fr-rjxr-66jc) involved escaping bypass, not the `Function()` call itself. The current version does not show additional template escaping bypass beyond the known prototype pollution findings.
- `lodash.js:422` — `Function('return this')()` as global-object detection. Not attacker-controlled input; no vulnerability.
- `lodash.js:437` — `nodeUtil` accesses `require('util').types`. Build-time env detection, not user input.
- `lodash.js:12837` — `defaultsDeep` delegation to `mergeWith`. Covered by CRITICAL finding above (filed at the root cause line 3640).
- `lodash.js:12787` — `_.defaults` assigns `object[key] = source[key]` for top-level keys only. The `__proto__` key is assigned as an own property, not as a prototype chain walk. `_.defaults({}, JSON.parse('{"__proto__":{"foo":true}}'))` does NOT pollute `Object.prototype` (confirmed by empirical test). The `constructor` key similarly creates an own property, not a polluted prototype.
- `lodash.js:2471` — `assignValue` delegates to `baseAssignValue` which has `__proto__` guard (line 2551). No gap beyond the CRITICAL `constructor` finding.
- `lodash.js:4766` — `copyObject` uses `keysIn` (includes inherited) to iterate source. Called by `assignIn`/`assignInWith`. Prototype pollution via `assignIn` not confirmed — `copyObject` uses `baseAssignValue` for new objects and `assignValue` for existing, both with `__proto__` guard. `constructor` key creates own property on the target, not prototype pollution.
- `lodash.js:13674` — `_.set` public entry. Filed as CRITICAL above.
- `lodash.js:13127` — `_.get` uses `baseGet` which reads via `object[toKey(path[index])]`. Read-only, no property write. Information leak only if prototype already polluted — not a pollution primitive itself.
- `lodash.js:8676` — `_.zipObjectDeep` delegates to `baseSet`. Filed as CRITICAL above.
- `lodash.js:3839` — `_.unset` / `baseUnset` uses path walk to find parent, then `delete`. The path can traverse `constructor.prototype`, but `delete Object.prototype[key]` requires the key to already exist on Object.prototype. Before pollution, this is a no-op; after pollution via another primitive, this is collateral. Not a standalone pollution vector.
- `lodash.js:13871` — `_.update` / `baseUpdate` delegates to `baseSet`. Filed as CRITICAL above.
- `lodash.js:12639` — `_.assignIn` uses `keysIn` to iterate inherited + own source properties, `copyObject` to copy. Creates own properties on target, not prototype chain manipulation.

## Dependencies

The `dependency_review` subagent reported 160 vulnerabilities across 1233 dependencies, all in **devDependencies only**. lodash has **zero runtime dependencies** — `lodash.js` is a fully self-contained IIFE with no `require()` calls to external packages. No vulnerable dependencies are shipped to end users.

Key devDependency vulns (not shipped):
- **handlebars**@4.0.6/4.0.11 — Arbitrary Code Execution (GHSA-2cf5-4w76-r9qv) — docdown build tooling only
- **fsevents**@1.0.15/1.2.2 — Malicious code (MAL-2023-462) — optional webpack dep, macOS-only
- **babel-traverse**@6.x — Arbitrary code execution (GHSA-67hx-6x53-jw92) — transpile tooling only
- **lodash**@3.10.1/4.17.3/4.17.9 — Self-referenced devDeps for compatibility testing, not shipped

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `lodash.js:3987` — Add blocklist for `__proto__`, `constructor`, `prototype` in `baseSet` path walk. Prevents prototype pollution via `_.set`, `_.setWith`, `_.update`, `_.updateWith`, `_.zipObjectDeep`.
2. `lodash.js:3640` — Extend `safeGet` to block `constructor` key (in addition to existing `__proto__` block). Prevents prototype pollution via `_.defaultsDeep` and `_.merge`.
3. `lodash.js:2550` — Add `constructor`/`prototype` to `baseAssignValue` guard (in addition to existing `__proto__` guard) as defense-in-depth for all property assignment paths.
4. `lodash.js:6616` — Add `constructor` to `safeGet` blocklist.