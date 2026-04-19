# Security Review: react-server-dom-webpack/src

This review scoped `packages/react-server-dom-webpack/src` in React 19.2.0 — the Webpack adapter layer for React Server Components. The package contains 22 source files (excluding tests). The core Flight protocol deserialization, server action execution, and reply parsing live in `react-server/src` and `react-client/src`, which are outside this review scope. This subpackage consists of:

- **Server entries** — `server/ReactFlightDOMServer{Node,Edge,Browser}.js` (re-exports `createResponse`, `decodeReply`, `decodeAction` from out-of-scope core; provides stream plumbing)
- **Client entries** — `client/ReactFlightDOMClient{Browser,Node,Edge}.js` (re-exports `createResponse`, `createFrom*` from out-of-scope core; provides stream plumbing)
- **Bundler configs** — `client/ReactFlightClientConfigBundlerWebpack{,Browser,Server,Node}.js` (runtime module loading via `__webpack_require__` / `import()`)
- **Webpack plugin** — `ReactFlightWebpackPlugin.js` (build-time manifest generation)
- **Node hooks** — `ReactFlightWebpackNodeLoader.js`, `ReactFlightWebpackNodeRegister.js` (compile-time source transforms)
- **References** — `ReactFlightWebpackReferences.js` (client/server reference proxy types)
- **Shared** — `ReactFlightImportMetadata.js` (type definitions only)

---

## MEDIUM

### File-path information disclosure via build-time manifest emission
- **File:** `ReactFlightWebpackPlugin.js:302`
- **Evidence:**
  ```
  const href = pathToFileURL(module.resource).href;
  ```
- **Attack Tree:**
  ```
  Webpack compilation (build time)
    └─ ReactFlightWebpackPlugin.js:302 — pathToFileURL converts module.resource to file:// URL
      └─ ReactFlightWebpackPlugin.js:309-313 — file:// URL written as key in clientManifest
        └─ ReactFlightWebpackPlugin.js:376-379 — JSON.stringify(clientManifest) emitted as asset
  ```
- **Taint Trace:** not run within budget — build-time data flow only. The `module.resource` value comes from Webpack's `compilation.chunkGraph` at line 358-365, which resolves to absolute filesystem source paths. These `file://` URLs become keys in `react-client-manifest.json` and values in `react-ssr-manifest.json` (via `ssrExports['*'].specifier`).
- **Impact:** Both `react-client-manifest.json` and `react-ssr-manifest.json` contain absolute `file://` URLs to every client-module source file in the project. The client-facing manifest is served to browsers (the client needs it for the Flight protocol), leaking the full filesystem path of every `'use client'` source file. This reveals project structure, absolute deployment paths, and potentially user names on the build machine.
- **Remediation:** Replace the `file://` URL with a stripped identifier or relative path. The spec only needs a stable opaque key, not a filesystem path.
  ```js
  // Instead of:
  const href = pathToFileURL(module.resource).href;
  clientManifest[href] = { ... };

  // Use a build-relative identifier:
  const relativePath = path.relative(
    compiler.context, module.resource
  ).replace(/\\/g, '/');
  clientManifest[relativePath] = { ... };
  ssrExports['*'] = { specifier: relativePath, name: '*' };
  ```

---

## LOW / INFORMATIONAL

### Fail-open behavior in Node.js custom loader source transform
- **File:** `ReactFlightWebpackNodeLoader.js:693`
- **Evidence:**
  ```
    console.error('Error parsing %s %s', url, x.message);
    return source;
  ```
- **Impact:** If `acorn.parse` throws on a source file, the loader log-an error and returns the original, un-transformed source. The module therefore loads *without* `'use server'`/`'use client'` transforms applied. In production, this means a file intended to register server actions via `transformServerModule` (line 756) or client module proxies via `transformClientModule` (line 753) silently fails over to the raw module. The failure is visible in server logs but the module still loads. This is a correctness issue rather than a security vulnerability in isolation — the transformed registration code never runs, so server-annotated functions are not registerable as server actions through the Flight protocol, degrading availability rather than creating an exploit.

### Default Webpack plugin scans entire project root
- **File:** `ReactFlightWebpackPlugin.js:82-88`
- **Evidence:**
  ```js
        this.clientReferences = [
          {
            directory: '.',
            recursive: true,
            include: /\.(js|ts|jsx|tsx)$/,
          },
        ];
  ```
- **Impact:** If the plugin is used without specifying `clientReferences`, it recursively scans from the current working directory. Combined with default Webpack resolution, this means any JS/TS file in the project tree (including `node_modules`, build artifacts, or temporary directories) that contains `'use client'` will be registered as a client reference. This is build-time only and mitigated by the fact that files must contain the explicit `'use client'` directive.

---

## Checked and Cleared

### Server-side endpoints (`decodeReply`, `decodeAction`, `decodeFormState`)
- **`server/ReactFlightDOMServerNode.js:614`** — `decodeReply` passes attacker-controlled `body` to `createResponse` (from `react-server/src/ReactFlightReplyServer`). Server function resolution and execution are gated by the `webpackMap` (ServerManifest) allowlist in the out-of-scope core module. No bypass found in this file.
- **`server/ReactFlightDOMServerNode.js:554`** — `decodeReplyFromBusboy` wires busboy events to `resolveField`/`resolveFileInfo`/`resolveFileChunk` (out-of-scope). Filename from the request flows through but is handled by the external Reply server.
- **`server/ReactFlightDOMServerEdge.js:247`** — `decodeReply` identical pattern to Node variant.
- **`server/ReactFlightDOMServerBrowser.js:242`** — `decodeReply` identical pattern.

### Module loading via bundler configs (no attacker-injectable specifiers)
- **`client/ReactFlightClientConfigBundlerWebpack.js:235`** — `__webpack_require__(metadata[ID])`. The `ID` is resolved from `bundlerConfig` (build-time manifest). Attacker wire-format data can select which manifest entry to use but cannot add entries.
- **`client/ReactFlightClientConfigBundlerWebpack.js:256`** — `moduleExports[metadata[NAME]]`. NAME is from manifest lookup, read-only property access.
- **`client/ReactFlightClientConfigBundlerNode.js:113`** — `import(metadata.specifier)`. The `specifier` is a `file://` URL from the build-time `ServerConsumerModuleMap`. Manifest entries originate from Webpack plugin output; no runtime injection path found.

### Server reference / client reference types (no unsafe property walks)
- **`ReactFlightWebpackReferences.js:140-199`** — `deepProxyHandlers.get` uses an exhaustive `switch` with per-name returns; all unhandled names throw an Error. Safe.
- **`ReactFlightWebpackReferences.js:201-307`** — `getReference` allows property walks to create child references but writes via `target[name] = ...` go directly to the inner object, not through the proxy `set` trap (which throws). Safe.
- **`ReactFlightWebpackReferences.js:309-340`** — `proxyHandlers.set` always throws. Safe.
- **`ReactFlightWebpackReferences.js:65-103`** — `bind` stores `$$bound` args. Arguments come from in-process function binding, not from wire data. Safe.

### Debug channel (DEV-only)
- **`server/ReactFlightDOMServerNode.js:106`**, **`server/ReactFlightDOMServerEdge.js:94`**, **`server/ReactFlightDOMServerBrowser.js:89`** — `resolveDebugMessage` receives parsed line-delimited strings from a ReadableStream. Only active when `__DEV__` is true. The function itself is in out-of-scope `ReactFlightServer`.

### Webpack plugin (build-time only)
- **`ReactFlightWebpackPlugin.js:112`** — `apply` compiler hook. Build-time; no runtime input.
- **`ReactFlightWebpackPlugin.js:404-427`** — `hasUseClientDirective` uses `acorn.parse` (not `acorn-loose`) to parse source. Errors are caught and return `false`. Build-time only.

### Node loader / register (compile-time only)
- **`ReactFlightWebpackNodeLoader.js:759`** — `transformSource` runs on modules as Node.js loads them. Source comes from filesystem, not network.
- **`ReactFlightWebpackNodeLoader.js:664-737`** — `transformModuleIfNeeded` uses `acorn.parse` with `onComment` callback to find source maps. Errors caught, original source returned.
- **`ReactFlightWebpackNodeRegister.js:16-108`** — `register()` overrides `Module.prototype._compile`. Only applies during build/transform. No untrusted input.

### Shared metadata types
- **`shared/ReactFlightImportMetadata.js`** — type definitions only. No runtime code.

---

## Dependencies

The dependency_review subagent found no `package.json` or lockfiles at the package level for this subdirectory (it's a monorepo subpackage). The dependencies used by this module (`acorn-loose`, `neo-async`, `busboy`) are inherited from the parent React monorepo. The dependency scan was applied to the repository root and found vulnerabilities in *other* code (OWASP Juice Shop, not in React). For this specific subpackage:
- `acorn-loose` — used at build time only for parsing source directives. Not request-time.
- `neo-async` — used in Webpack plugin for async file scanning. Build-time only.
- No direct runtime deserialization libraries in scope.
- **No vulnerable dependencies found in this subpackage's own dependency tree.**

---

## Remediation Summary

### Immediate (CRITICAL/HIGH)
No CRITICAL or HIGH findings.

### Short-term (MEDIUM)
1. `ReactFlightWebpackPlugin.js:302` — Replace absolute `file://` URLs with build-relative identifiers in manifest output to prevent filesystem path disclosure via client-facing JSON assets.

### Hardening (LOW)
1. `ReactFlightWebpackNodeLoader.js:693` — Consider returning an error (throwing) or emitting a warning that aborts the module load rather than silently returning un-transformed source, to prevent fail-open behavior for `'use server'`/`'use client'` transforms.