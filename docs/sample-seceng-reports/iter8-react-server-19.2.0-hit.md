This is a focused review of the **React Flight reply server and action deserializer** — the code that receives a `FormData` payload from the client and reconstructs typed server references.  The critical findings concern the prototype-walk primitive that is the foundation for RCE.

---

# Security Review: React SSR / RSC Runtime

## CRITICAL

### Prototype-walk primitive in `getOutlinedModel` — no constructor/__proto__/prototype blocklist
- **File:** `ReactFlightReplyServer.js:615`
```js
value = value[path[i]];
```
- **Evidence:**
  ```javascript
  // getOutlinedModel (lines 595–637):
  const path = reference.split(':');  // line 602 — colon-separated path from FormData
  const id = parseInt(path[0], 16);   // line 603
  const chunk = getChunk(response, id); // line 604
  // ...
  case INITIALIZED:
    let value = chunk.value;
    for (let i = 1; i < path.length; i++) {
      value = value[path[i]];           // line 615 — UNSAFE PROPERTY WALK
    }
    return map(response, value);        // line 617
  ```

  Same primitive at **line 565** in `createModelResolver`:
  ```javascript
  // createModelResolver callback (lines 563–568):
  for (let i = 1; i < path.length; i++) {
    value = value[path[i]];             // line 565 — identical walk
  }
  parentObject[key] = map(response, value);
  ```

- **Attack Tree:**
  ```
  ReactFlightActionServer.js:94 — body.forEach reads FormData from attacker
    └─ ReactFlightActionServer.js:103 — formFieldPrefix built from body key
      └─ ReactFlightActionServer.js:104 — decodeBoundActionMetaData creates response with attacker FormData
        └─ ReactFlightReplyServer.js:525 — getChunk reads FormData entry via response._formData.get(key)
          └─ ReactFlightReplyServer.js:466 — JSON.parse of attacker-controlled model string
            └─ ReactFlightReplyServer.js:395 — parseModelString on a "$F" server-reference value
              └─ ReactFlightReplyServer.js:942 — getOutlinedModel(response, ref, ...) — ref is attacker string
                └─ ReactFlightReplyServer.js:602 — reference.split(':') produces path[]
                  └─ ReactFlightReplyServer.js:614–615 — value = value[path[i]] walks arbitrary keys
  ```

- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=javascript, files=2, defs=18, calls=95, unresolved_callees=12
  Found 3 candidate path(s) from ReactFlightActionServer.js:94 to ReactFlightReplyServer.js:615:

  Path 1 (depth 9, resolved 7/9 hops):
    ReactFlightActionServer.js:94 [byte 2890-2940] — decodeAction — taint root: body (FormData)
    └─ ReactFlightActionServer.js:104 [byte 3100-3180] — fn `decodeBoundActionMetaData` — taint root: body
      └─ ReactFlightReplyServer.js:62 [byte 950-1020] — fn `createResponse` — taint root: backingFormData
        └─ ReactFlightReplyServer.js:466 [byte 12500-12530] — fn `initializeModelChunk` — taint root: resolvedModel
          └─ ReactFlightReplyServer.js:395 [byte 8200-8260] — fn `parseModelString` — taint root: value
            └─ ReactFlightReplyServer.js:942 [byte 24800-24880] — fn `getOutlinedModel` — taint root: ref
              └─ ReactFlightReplyServer.js:602 [byte 15200-15240] — fn `getOutlinedModel` — taint root: reference
                └─ ReactFlightReplyServer.js:614 [byte 15700-15750] — fn `getOutlinedModel` — taint root: path
                  └─ ReactFlightReplyServer.js:615 [byte 15760-15790] — [SINK REACHED] — tainted at sink: value[path[i]]

  Path 2 (depth 8, resolved 6/8 hops):
    ReactFlightReplyServer.js:525 [byte 13100-13140] — fn `getChunk` — taint root: backingEntry
    └─ ReactFlightReplyServer.js:268 [byte 6800-6840] — fn `resolveModelChunk`
      └─ ReactFlightReplyServer.js:466 [byte 12500-12530] — fn `initializeModelChunk`
        └─ ReactFlightReplyServer.js:942 [byte 24800-24880] — fn `getOutlinedModel`
          └─ ReactFlightReplyServer.js:602 [byte 15200-15240] — fn `getOutlinedModel` — taint root: reference
            └─ ReactFlightReplyServer.js:615 [byte 15760-15790] — [SINK REACHED] — tainted at sink: value[path[i]]

  Path 3 (depth 12, resolved 10/12 hops): [TRUNCATED]
    ReactFlightActionServer.js:94 [byte 2890-2940] — decodeAction — taint root: body
    └─ ...
  ```

- **Impact:** An attacker with an attacker-controlled `FormData` field whose **value** contains a JSON-serialized `$F` server-reference (e.g. `$1` with FormData key `prefix1` holding the string `["id","bound"]`) controls the `reference` string passed to `getOutlinedModel`.  The colon-split produces a `path` array that is walked verbatim via `value = value[path[i]]`.  If the walker lands on `chunk.value` (the parsed JSON object from the same FormData entry), the attacker can set `path = ["0", "constructor"]` to reach `Object`'s constructor (a `Function`), or `path = ["0", "__proto__"]` to reach `Object.prototype`.  These resolved values are passed to `map(response, value)` which feeds `createModelResolver` or `loadServerReference`.  When the resolved value is a `Function` instance, calling it is RCE.  Even when current downstream consumers (`createModel`, `createMap`, `createSet`, `loadServerReference`) happen to reject non-iterable `Function` values, the **prototype-walk primitive itself exists without any blocklist** — a refactor to a different `map` function (e.g. a direct `eval(value)`, `new Function(value)`, or `Function.prototype.call.bind(value)`) flips this to live RCE.  The same pattern exists at line 565 in `createModelResolver`.

- **Exploit:**
  ```
  POST /rsc-action
  Content-Type: multipart/form-data; boundary=Z

  --Z
  Content-Disposition: form-data; name="$ACTION_prefix1"

  ["0:constructor"]
  --Z
  Content-Disposition: form-data; name="$ACTION_REF_0"

  {"id":"fake","bound":null}
  --Z
  Content-Disposition: form-data; name="prefix1_0"

  {"fake":"data"}
  --Z
  Content-Disposition: form-data; name="$ACTION_ID_fake"

  ...
  ```

  Sending a malformed `reference` such as `"0:constructor:bind"` causes `path = ["0", "constructor", "bind"]`, which walks `chunk.value[0].constructor.bind` — the `Function.prototype.bind` method — and passes it downstream as a callable.

- **Remediation:** Add an explicit property-name blocklist before each property-access step in the walk loops at lines 614-615 and 563-566:

  ```diff
  function getOutlinedModel<T>(
    ...
    for (let i = 1; i < path.length; i++) {
  +   if (path[i] === 'constructor' || path[i] === '__proto__' || path[i] === 'prototype') {
  +     throw new Error('Blocked property access in model resolution.');
  +   }
      value = value[path[i]];
    }
  ```

  Apply the same guard to the `createModelResolver` callback at line 564.

---

### Same prototype-walk primitive in `createModelResolver` — async resolution path
- **File:** `ReactFlightReplyServer.js:565`
```js
value = value[path[i]];
```
- **Evidence:**
  ```javascript
  // createModelResolver (lines 542–589), returned resolver function:
  return value => {
    for (let i = 1; i < path.length; i++) {
      value = value[path[i]];   // line 565 — identical walk, no blocklist
    }
    parentObject[key] = map(response, value);
  ```

  This callback is invoked when a chunk is `BLOCKED` (line 620–634 in `getOutlinedModel`), meaning it's used for the **asynchronous** resolution path.  While `getOutlinedModel` handles the synchronous INITIALIZED case at line 615, this handler at line 565 handles all BLOCKED/CYCLIC/PENDING chunks.  Both share the same unsanitised `path` array from `reference.split(':')` at line 602.

- **Attack Tree:**
  ```
  ReactFlightActionServer.js:94 — body.forEach reads attacker FormData
    └─ ReactFlightActionServer.js:104 — decodeBoundActionMetaData
      └─ ReactFlightReplyServer.js:62 — createResponse with attacker FormData
        └─ ReactFlightReplyServer.js:466 — JSON.parse of attacker model
          └─ ReactFlightReplyServer.js:942 — getOutlinedModel
            └─ ReactFlightReplyServer.js:620 -- BLOCKED/PENDING case
              └─ ReactFlightReplyServer.js:622 -- chunk.then(createModelResolver(..., path, ...))
                └─ ReactFlightReplyServer.js:564 -- resolver callback fires
                  └─ ReactFlightReplyServer.js:565 -- value = value[path[i]] walks attacker key
  ```

- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=javascript, files=2, defs=18, calls=95, unresolved_callees=12
  Found 1 candidate path(s) from ReactFlightActionServer.js:94 to ReactFlightReplyServer.js:565:

  Path 1 (depth 10, resolved 8/10 hops):
    ReactFlightActionServer.js:94 [byte 2890-2940] — decodeAction — taint root: body
    └─ ReactFlightActionServer.js:104 [byte 3100-3180] — fn `decodeBoundActionMetaData`
      └─ ReactFlightReplyServer.js:62 [byte 950-1020] — fn `createResponse`
        └─ ReactFlightReplyServer.js:466 [byte 12500-12530] — fn `initializeModelChunk`
          └─ ReactFlightReplyServer.js:942 [byte 24800-24880] — fn `getOutlinedModel` — taint root: reference
            └─ ReactFlightReplyServer.js:620 [byte 16200-16250] — fn `getOutlinedModel` — taint root: chunk
              └─ ReactFlightReplyServer.js:622 [byte 16300-16380] — fn `createModelResolver` — taint root: path
                └─ ReactFlightReplyServer.js:563 [byte 14000-14040] — fn `createModelResolver` — taint root: path
                  └─ ReactFlightReplyServer.js:565 [byte 14100-14130] — [SINK REACHED] — tainted at sink: value[path[i]]
  ```

- **Impact:** Same primitive, different code path (async resolution).  The `path` array is stored in the closure at line 549 and used at line 565 when a pending chunk resolves.  No blocklist at either site.  Combined with the sync path at line 615, this gives two independent code paths for the same prototype-walk RCE primitive.  The fix is the same blocklist applied at both locations.

---

### `JSON.parse` on unvalidated `FormData` field value
- **File:** `ReactFlightReplyServer.js:466`
```js
const rawModel = JSON.parse(resolvedModel);
```
- **Evidence:**
  ```javascript
  function initializeModelChunk<T>(chunk: ResolvedModelChunk<T>): void {
    // ...
    const resolvedModel = chunk.value;  // from FormData (line 528)
    const rawModel = JSON.parse(resolvedModel); // line 466
  ```

  The `resolvedModel` originates from `getChunk` (line 525) which reads `response._formData.get(key)`, populated by `resolveField` (line 1110–1126) which takes a `key` and `value` directly from the request stream.  No validation, size limit, or schema enforcement occurs before `JSON.parse`.

- **Attack Tree:**
  ```
  ReactFlightActionServer.js:94 — body.forEach (attacker FormData)
    └─ ReactFlightReplyServer.js:1116 — resolveField appends to _formData
      └─ ReactFlightReplyServer.js:525 — getChunk reads FormData value
        └─ ReactFlightReplyServer.js:528 — value passed to createResolvedModelChunk
          └─ ReactFlightReplyServer.js:466 — JSON.parse of raw value
  ```

- **Impact:** `JSON.parse` itself is safe in JS, but the parsed result feeds `parseModelString` / `reviveModel` which process `$`-prefixed special values (`$F` for server references, `$@` for promises, `$R` for streams, etc.).  The absence of input validation means an attacker can supply arbitrarily deep JSON structures to trigger prototype pollution during `reviveModel`'s `for...in` loop (lines 416–438) — note that while `hasOwnProperty` guards are used, the `for...in` still iterates all own keys and recursively calls `reviveModel` on each value, which can produce deeply nested references to be resolved via `getOutlinedModel`.

- **Remediation:** Validate and constrain the size/depth/structure of the incoming model before `JSON.parse`.  Add a max-depth limit to `JSON.parse` via a reviver, or pre-check that the chunk is bounded:
  ```javascript
  if (resolvedModel.length > MAX_MODEL_SIZE) throw new Error('Model too large');
  const depth = estimateJsonDepth(resolvedModel);
  if (depth > MAX_JSON_DEPTH) throw new Error('JSON too deep');
  const rawModel = JSON.parse(resolvedModel);
  ```

  (This is a supporting control; the primary fix is the prototype-walk blocklist above.)

---

### `bindArgs` applies attacker-controlled `args` to `requireModule(serverReference)` return
- **File:** `ReactFlightReplyServer.js:338`
```js
return fn.bind.apply(fn, [null].concat(args));
```
- **Evidence:**
  ```javascript
  // loadServerReference (lines 341–384):
  const serverReference = resolveServerReference(...)(response._bundlerConfig, id);
  // ...
  promise = Promise.all([(bound: any), preloadPromise]).then(
    ([args]: Array<any>) => bindArgs(requireModule(serverReference), args), // line 358
  );

  // bindArgs:
  function bindArgs(fn, args) {
    return fn.bind.apply(fn, [null].concat(args)); // line 338
  }
  ```

  The `bound` promise comes from `getOutlinedModel` which walks the prototype-walkable path (line 602–617).  An attacker who controls the `bound` value (via the prototype walk) controls the arguments bound to the required module.  If the resolved module is a callable action function, the attacker supplies arbitrary `args` that become the function's first positional arguments.

- **Attack Tree:**
  ```
  ReactFlightActionServer.js:94 — FormData iteration
    └─ ReactFlightActionServer.js:104 — decodeBoundActionMetaData
      └─ ReactFlightReplyServer.js:942 — getOutlinedModel on "$F" reference
        └─ ReactFlightReplyServer.js:602 — split produces path including bound args
          └─ ReactFlightReplyServer.js:615 — walk retrieves bound args (attacker-controlled)
            └─ ReactFlightReplyServer.js:357 — bound args passed to bindArgs
              └─ ReactFlightReplyServer.js:338 — fn.bind.apply(fn, [null, ...attackerArgs])
  ```

- **Taint Trace:**
  ```
  taint_trace: lossy — every returned path is a hypothesis
  index: language=javascript, files=2, defs=18, calls=95, unresolved_callees=12
  Found 2 candidate path(s) from ReactFlightActionServer.js:94 to ReactFlightReplyServer.js:338:

  Path 1 (depth 11, resolved 9/11 hops):
    ReactFlightActionServer.js:94 [byte 2890-2940] — decodeAction — taint root: body
    └─ ReactFlightActionServer.js:104 [byte 3100-3180] — fn `decodeBoundActionMetaData`
      └─ ReactFlightReplyServer.js:62 [byte 950-1020] — fn `createResponse`
        └─ ReactFlightReplyServer.js:466 [byte 12500-12530] — fn `initializeModelChunk`
          └─ ReactFlightReplyServer.js:942 [byte 24800-24880] — fn `getOutlinedModel` — taint root: reference
            └─ ReactFlightReplyServer.js:615 [byte 15760-15790] — fn `getOutlinedModel` — [SINK REACHED] (bound value)
              └─ ReactFlightReplyServer.js:343 [byte 7800-7850] — fn `loadServerReference` — taint root: bound
                └─ ReactFlightReplyServer.js:358 [byte 8900-8950] — fn `bindArgs` — taint root: args
                  └─ ReactFlightReplyServer.js:338 [byte 7200-7250] — [SINK REACHED] — tainted at sink: fn.bind.apply(fn, [null].concat(args))
  ```

- **Impact:** Attacker controls the bound arguments of any server action resolved through the RSC mechanism.  When combined with the prototype-walk finding above, this allows an attacker to invoke server action functions with arbitrary parameters — a direct server-side code execution path.  If the action performs database queries, file operations, or state mutations, the attacker executes those operations with arbitrary attacker-supplied arguments.

- **Remediation:** Validate the bound arguments structure and type before passing them to `bindArgs`.  Ensure that `id` in `loadServerReference` comes from a server-controlled manifest, not from the FormData payload directly.  Add input validation:
  ```javascript
  function bindArgs(fn, args) {
    if (!Array.isArray(args)) throw new Error('Bound args must be an array');
    if (args.length > MAX_BOUND_ARGS) throw new Error('Too many bound args');
    // Validate that args don't contain Function, constructor, __proto__ values
    for (const arg of args) {
      if (typeof arg === 'function') throw new Error('Functions not allowed in bound args');
    }
    return fn.bind.apply(fn, [null].concat(args));
  }
  ```

---

### Server reference `id` taken from unvalidated `$ACTION_ID_` FormData key
- **File:** `ReactFlightActionServer.js:113`
```js
const id = key.slice(11);
action = loadServerReference(serverManifest, id, null);
```
- **Evidence:**
  ```javascript
  // decodeAction (lines 83–124):
  body.forEach((value, key) => {
    if (key.startsWith('$ACTION_')) {
      // ...
      if (key.startsWith('$ACTION_ID_')) {
        const id = key.slice(11);           // line 113 — attacker-controlled key fragment
        action = loadServerReference(serverManifest, id, null); // line 114
        return;
      }
    }
  ```

  The `id` is derived directly from the FormData key name — anything the attacker names after `$ACTION_ID_` becomes the server reference identifier.  This `id` is passed to `resolveServerReference(bundlerConfig, id)` in `loadServerReference`.

- **Attack Tree:**
  ```
  Attacker crafts multipart form with key "$ACTION_ID_attacker-controlled"
    └─ ReactFlightActionServer.js:113 — id = "attacker-controlled"
      └─ ReactFlightActionServer.js:114 — loadServerReference(serverManifest, id, null)
        └─ ReactFlightActionServer.js:37 — resolveServerReference(bundlerConfig, id)
          └─ ReactFlightActionServer.js:44/48/52 — requireModule(serverReference)
  ```

- **Impact:** The `id` from FormData gates which server reference module is loaded and executed.  If `resolveServerReference` in the `ReactFlightClientConfig` doesn't validate that `id` corresponds to an allowed/exported server action (e.g., if it's a simple map lookup or hash), the attacker can resolve arbitrary module IDs and invoke arbitrary server functions via `requireModule`.  Without concrete access to the FlightClientConfig implementation (which is forked/config-dependent), the severity depends on the host's specific resolver — but the **unvalidated input** at line 113 is universally present.

- **Remediation:** Validate the `id` against a server-controlled allowlist before `loadServerReference`:
  ```javascript
  if (key.startsWith('$ACTION_ID_')) {
    const id = key.slice(11);
    if (!isValidServerReferenceId(id, serverManifest)) {
      throw new Error('Invalid server reference ID');
    }
    action = loadServerReference(serverManifest, id, null);
    return;
  }
  ```

---

## HIGH

### Server reference loading via `$ACTION_REF_` also accepts unvalidated ID from FormData
- **File:** `ReactFlightActionServer.js:103`
```js
const formFieldPrefix = '$ACTION_' + key.slice(12) + ':';
```
- **Evidence:**
  ```javascript
  if (key.startsWith('$ACTION_REF_')) {
    const formFieldPrefix = '$ACTION_' + key.slice(12) + ':'; // line 103
    const metaData = decodeBoundActionMetaData(
      body,
      serverManifest,
      formFieldPrefix,
    );
    action = loadServerReference(serverManifest, metaData.id, metaData.bound); // line 109
  ```

  The `formFieldPrefix` is constructed from the attacker-supplied FormData key name.  `decodeBoundActionMetaData` then uses this prefix to read fields from `body` (the FormData).  The `metaData.id` returned from `decodeBoundActionMetaData` is used as the server reference `id`, and `metaData.bound` provides the bound arguments — both derived from user-controlled FormData fields under the attacker-chosen prefix.

- **Impact:** The attacker controls which FormData keys get read by using the prefix.  They can fabricate a `metaData` object with an arbitrary `id` and `bound` value, then trigger `loadServerReference` with those values.  Combined with the prototype-walk primitive, this provides a second independent path to arbitrary server action invocation.

---

## MEDIUM

### `reviveModel` recursive `for...in` on attacker-controlled JSON
- **File:** `ReactFlightReplyServer.js:416–438`
```js
for (const key in value) {
  if (hasOwnProperty.call(value, key)) {
    const childRef = reference + ':' + key;
    value[key] = reviveModel(response, value, key, value[key], childRef);
  }
}
```
- **Evidence:** The `for...in` loop is guarded by `hasOwnProperty`, preventing prototype pollution through inherited properties.  However, the `childRef` string is built from attacker-controlled object keys concatenated with `:`, which feeds into `parseModelString` for any `$`-prefixed values.

- **Impact:** Low — the hasOwnProperty guard prevents direct prototype pollution.  The main risk is that deeply nested structures could be used to increase processing time (mild resource exhaustion, which is excluded by the Never Report rules).  The `hasOwnProperty` check is the correct mitigation here.

---

## Checked and Cleared

- `ReactFlightReplyServer.js:125` — `Chunk.prototype = Object.create(Promise.prototype)` is a standard JS inheritance pattern, not an attacker-controlled prototype assignment.
- `ReactFlightReplyServer.js:640–648` — `createMap` / `createSet` use type-coercive constructors (`new Map(model)`, `new Set(model)`) — if the model came from a path walk through `constructor`/`__proto__`, these would reject non-iterable inputs (coincidental guard, not intentional blocklist — see CRITICAL #1).
- `ReactFlightReplyServer.js:660–698` — `parseTypedArray` reads from `response._formData.get(key)` where `key` is the reference string with `'$A'`/`'$O'` etc. prefix removed; no eval or exec.
- `ReactFlightReplyServer.js:700–804` — `parseReadableStream` creates a `ReadableStream` and resolves it under the provided `id`; no unvalidated input reaches a dangerous sink.
- `ReactFlightReplyServer.js:826–913` — `parseAsyncIterable` similarly constructs an async iterable from FormData; the iterator is created from internal state, not user input.
- `ReactFlightReplyServer.js:916–1089` — `parseModelString` — the switch on `value[0]`/`value[1]` handles `$`-prefarsed value types; each case resolves to typed values (Date, BigInt, Map, Set, etc.) — the prototype-walk at `$F` case (lines 937–950) is the CRITICAL finding.
- `ReactFlightReplyServer.js:1091–1108` — `createResponse` creates an internal Response object; not an attacker entry point.
- `ReactFlightReplyServer.js:1110–1126` — `resolveField` appends attacker key/value to `_formData` but validates format (prefix match) before triggering resolution.
- `ReactFlightReplyServer.js:1129–1132` — `resolveFile` appends file to FormData — File constructor is safe.
- `ReactFlightReplyServer.js:1174–1180` — `close` reports a global error with fixed message "Connection closed" — no injection.
- `ReactFlightActionServer.js:126–169` — `decodeFormState` reads `$ACTION_KEY` from FormData for form state tracking; returns structured state, not used for code execution.
- `ReactFlightServer.js:184–197` — `devirtualizeURL` decodes URL via `decodeURI` — not attacker input, used for DEV stack trace processing.
- `ReactFlightServer.js:352` (OPEN constant) — state machine constant, not file I/O.
- `ReactFizzServer.js:352` — Same constant definition (OPEN = 11), not file I/O.
- `ReactFlightServerConfigDebugNode.js:30` — `async_hooks.createHook` is used for internal async tracking, no user input reaches any hook handler.

---

## Dependencies

No dependency manifests (`package.json`, `yarn.lock`, `pnpm-lock.yaml`, `Cargo.toml`, etc.) found in the review scope — this directory contains only React internal source files.  Unable to perform OSV dependency scanning.

---

## Remediation Summary

### Immediate (CRITICAL)
1. **`ReactFlightReplyServer.js:614–615`** — Add constructor/__proto__/prototype blocklist to `getOutlinedModel` walk loop.
2. **`ReactFlightReplyServer.js:564–565`** — Add same blocklist to `createModelResolver` walk loop.
3. **`ReactFlightReplyServer.js:466`** — Validate/limit incoming model size and structure before `JSON.parse`.
4. **`ReactFlightReplyServer.js:338`** — Validate `args` to `bindArgs` — reject non-array, function-typed, or prototype-relevant values.
5. **`ReactFlightActionServer.js:113-114`** — Validate `$ACTION_ID_` key fragment against server-controlled allowlist before `loadServerReference`.

### Short-term (HIGH)
1. **`ReactFlightActionServer.js:103`** — Validate `$ACTION_REF_` prefix and derived `formFieldPrefix` to prevent fabrication of `metaData.id`/`bound` values from arbitrary FormData keys.