I have a complete picture now. This is a build-time / library package with no runtime network endpoints or direct deserialization sinks. The RSC reply parsing (`JSON.parse`-level deserialization) lives in the `react-server` and `react-client` packages, not in `react-server-dom-webpack/src`.

The actual content of this package is:
- **Build-time**: Webpack plugin, Node ESM loader, Node register hook
- **Runtime**: Client/Server entry points that delegate to `react-server` and `react-client` for the actual serialization/deserialization

There are no runtime HTTP handlers, no deserialization of attacker-controlled data within this package itself, and no injection sinks. The `decodeReply*` functions just wrap `createResponse` / `resolveField` from `react-server/src/ReactFlightReplyServer` which is outside scope.

Let me write the final report:

# Security Review: react-server-dom-webpack (v19.2.0)

## Scope Summary

The package `packages/react-server-dom-webpack/src` contains **23 non-test files** (4 top-level source files + 9 client files + 9 server files + 1 shared file). It provides:
- **Build-time**: Webpack plugin (`ReactFlightWebpackPlugin.js`), Node ESM loader (`ReactFlightWebpackNodeLoader.js`), Node require hook (`ReactFlightWebpackNodeRegister.js`)
- **Runtime**: Client/Server entry points for Node, Edge, Browser — thin wrappers delegating to `react-server` and `react-client` packages for actual RSC serialization/deserialization

## Findings

### MEDIUM

### Hardcoded directory traversal scan in Webpack plugin default config
- **File:** `ReactFlightWebpackPlugin.js:84`
- **Evidence:**
  ```js
  directory: '.',
  recursive: true,
  include: /\.(js|ts|jsx|tsx)$/,
  ```
- **Attack Tree:**
  ```
  ReactFlightWebpackPlugin.js:81-88 — constructor defaults clientReferences to { directory: '.', recursive: true }
    └─ ReactFlightWebpackPlugin.js:444-510 — resolveAllClientFiles recurses CWD, reading every matching source file
      └─ ReactFlightWebpackPlugin.js:491 — fs.readFile reads content to check for 'use client' directive
  ```
- **Taint Trace:** not run within budget — design-level finding, no external taint source
- **Impact:** If a developer uses the plugin without specifying `clientReferences`, it recursively scans and reads all `.js`/`.ts`/`.jsx`/`.tsx` files from the current working directory. In a monorepo or container where CWD contains sibling packages' source code (including `node_modules` or secret-bearing files if extensions match), the build process reads files the developer did not intend to include. This is a build-time concern, not a runtime vulnerability.
- **Remediation:** Remove the default and require explicit `clientReferences` configuration, or narrow the default to a project-specific directory:
  ```js
  import {isAbsolute} from 'path';
  // In the constructor:
  if (!options.clientReferences) {
    throw new Error(
      PLUGIN_NAME + ': You must specify clientReferences. ' +
      'For backwards compatibility use { directory: process.cwd(), recursive: true, ... }.',
    );
  }
  ```

## Checked and Cleared

- `ReactFlightWebpackReferences.js:37-46` — `registerClientReference`: assigns developer-provided `id + '#' + exportName` strings to `$$id` property. Not attacker-controlled at runtime.
- `ReactFlightWebpackReferences.js:105-136` — `registerServerReference`: same pattern, build-time only.
- `ReactFlightWebpackReferences.js:140-199` — `deepProxyHandlers` (client module proxy `get` trap): exhaustive case/switch on property name, rejects all unknown strings with `throw new Error('Cannot dot into a client module...')`. No prototype walk — `constructor` / `__proto__` / `prototype` are not special-cased but fall through to the error throw on line 190. Verified: the `switch` has no `default` branch for unknown string names; line 189-193 throws unconditionally.
- `ReactFlightWebpackReferences.js:201-307` — `getReference`: same pattern — only known property names (`$$typeof`, `$$id`, `$$async`, `name`, `defaultProps`, `_debugInfo`, `toJSON`, `__esModule`, `then`, symbols) are handled; all other string names fall through to line 287 where `target[name]` is a property assignment (not a walk), and the result is wrapped in a proxy that itself only throws when invoked.
- `ReactFlightWebpackReferences.js:309-340` — `proxyHandlers.get/set`: same pattern, delegates to `getReference` which throws for unknown names. Set trap always throws.
- `ReactFlightWebpackNodeRegister.js:16-108` — `register()` Node require hook: patches `Module._compile` to parse source for `use client`/`use server` directives. Only operates on developer-authored modules loaded by `require()`. No external input.
- `ReactFlightWebpackNodeLoader.js:66-89` — `resolve()`: adds `react-server` condition to import contexts. Standard loader behavior.
- `ReactFlightWebpackNodeLoader.js:91-99` — `getSource()`: passthrough to `defaultGetSource`.
- `ReactFlightWebpackNodeLoader.js:184-458` — `transformServerModule()`: AST-level analysis of `export` declarations to inject `registerServerReference` calls. Operates on developer-authored source. `JSON.parse` at line 743 parses a source map loaded via `loader()` (build-time).
- `ReactFlightWebpackNodeLoader.js:471-569` — `parseExportNamesInto()`: recursive AST analysis of `export *` statements. Build-time only.
- `ReactFlightWebpackNodeLoader.js:491-507` — `resolveClientImport()`: resolves specifiers using stashed `resolve()` function. Build-time.
- `ReactFlightWebpackNodeLoader.js:571-623` — `transformClientModule()`: generates client reference proxy source. Build-time only.
- `ReactFlightWebpackNodeLoader.js:625-646` — `loadClientImport()`: loads transformed client module source. Build-time.
- `ReactFlightWebpackNodeLoader.js:648-757` — `transformModuleIfNeeded()`: main transform entry point. Checks for `use client`/`use server` directives. Build-time.
- `ReactFlightWebpackNodeLoader.js:759-784` — `transformSource()`: Node ESM loader hook. Call-time only.
- `ReactFlightWebpackNodeLoader.js:786-804` — `load()`: Node ESM loader hook. Build-time only.
- `ReactFlightWebpackPlugin.js:66-525` — Webpack plugin: scans file system for client references, generates `clientManifest` and `serverConsumerManifest` JSON assets. Build-time only. `fs.readFile` at line 491 reads developer-owned source files.
- `client/ReactFlightClientConfigBundlerWebpack.js:69-112` — `resolveClientReference`: looks up `bundlerConfig[metadata[ID]]` and `moduleExports[metadata[NAME]]`. `bundlerConfig` is the server consumer manifest (build-generated, trusted). `metadata` comes from parsed flight data but the lookup result is validated by the manifest schema.
- `client/ReactFlightClientConfigBundlerWebpack.js:114-156` — `resolveServerReference`: splits `id` on `#` and looks up `bundlerConfig[id]`. Server manifest is build-generated.
- `client/ReactFlightClientConfigBundlerWebpack.js:164-189` — `requireAsyncModule(id)`: `__webpack_require__(id)` where `id` comes from `metadata[ID]` which is resolved from the manifest. Webpack `__webpack_require__` only resolves bundled module IDs — not arbitrary file paths.
- `client/ReactFlightClientConfigBundlerWebpack.js:234-257` — `requireModule(metadata)`: `__webpack_require__(metadata[ID])` and `moduleExports[metadata[NAME]]` — both resolved from build-time manifest.
- `client/ReactFlightClientConfigBundlerNode.js:60-88` — `resolveClientReference`: returns `{ specifier: resolvedModuleData.specifier, name }` from trusted manifest.
- `client/ReactFlightClientConfigBundlerNode.js:90-98` — `resolveServerReference`: returns `{ specifier: id.slice(0, idx), name: id.slice(idx + 1) }` where `id` is a server reference ID. For the **node bundler config**, this is used by `preloadModule` to `import(metadata.specifier)` at line 113.
- `client/ReactFlightClientConfigBundlerNode.js:102-139` — `preloadModule`: `import(metadata.specifier)` where `specifier` comes from `resolveServerReference` / `resolveClientReference`. In **server-side rendering** (server consuming the manifest), the specifier is the path from the build-time manifest. In **edge runtime**, the node bundler is not used.
- `client/ReactFlightClientConfigBundlerNode.js:141-162` — `requireModule`: property access `moduleExports[metadata.name]` where `moduleExports` is the loaded module.
- `client/ReactFlightClientConfigBundlerNode.js:102-139` — `preloadModule`: `import(metadata.specifier)` at line 113 — the specifier originates from `resolveClientReference` or `resolveServerReference`, both of which pull from the build-time manifest. The manifest author (developer) controls the values.
- `client/ReactFlightClientConfigTargetWebpackBrowser.js` — empty, no logic.
- `client/ReactFlightClientConfigTargetWebpackServer.js:14` — type definition for `crossOrigin`. No executable logic.
- `client/ReactFlightDOMClientBrowser.js` — thin wrapper delegating to `react-client`. No sinks.
- `client/ReactFlightDOMClientEdge.js` — thin wrapper delegating to `react-client`. No sinks.
- `client/ReactFlightDOMClientNode.js` — thin wrapper delegating to `react-client`. No sinks.
- `client/ReactFlightClientConfigBundlerWebpackBrowser.js` — `loadChunk` / `addChunkDebugInfo`. No sinks.
- `client/ReactFlightClientConfigBundlerWebpackServer.js` — `loadChunk` / `addChunkDebugInfo`. No sinks.
- `client/react-flight-dom-client.*.js` — re-export barrel files. No logic.
- `server/ReactFlightServerConfigWebpackBundler.js` — type definitions and re-exports only. No sinks.
- `server/ReactFlightDOMServerNode.js:554-612` — `decodeReplyFromBusboy`: wraps `busboy` stream to call `resolveField`/`resolveFileInfo`/`resolveFileChunk` from `react-server/src/ReactFlightReplyServer`. No sinks in this package.
- `server/ReactFlightDOMServerNode.js:614-633` — `decodeReply`: creates `FormData`, calls `createResponse` / `getRoot` from `react-server`. No sinks in this package.
- `server/ReactFlightDOMServerNode.js:635-678` — `decodeReplyFromAsyncIterable`: same pattern. No sinks in this package.
- `server/ReactFlightDOMServerEdge.js` — same patterns as Node server. Delegates to `react-server` package.
- `server/ReactFlightDOMServerBrowser.js` — same patterns. Delegates to `react-server` package.
- `server/react-flight-dom-server.*.js` — barrel files. No logic.
- `shared/ReactFlightImportMetadata.js` — type definitions and constants only.
- `__tests__/*` — test files. Excluded per review policy.

## Dependencies

No lockfiles detected in the repo. The `dependency_review` subagent reported no npm/yarn/pnpm lockfiles. No vulnerable dependencies identified within this package's scope. (Note: `acorn-loose` is used for parsing at build-time — no security impact from parsing loose JavaScript during development.)

## Remediation Summary

### Hardening (MEDIUM)
1. `ReactFlightWebpackPlugin.js:84` — Remove the default CWD-scan for `clientReferences` or narrow it to an explicit project directory. Require `clientReferences` to be specified.