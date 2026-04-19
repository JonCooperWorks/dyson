# Security Review: React Server Components Webpack Adapter

## Assessment

This package (`react-server-dom-webpack`) is a **build-time adapter** that wires React Server Components into Webpack and Node.js ESM loaders. After reviewing all 13 non-test source files, the security-sensitive operations (reply deserialization, server reference resolution, action invocation, chunk parsing) are implemented in the external `react-server/` and `react-client/` packages and are not present in this adapter.

This adapter operates in three phases:

1. **Build-time** (`ReactFlightWebpackPlugin.js`): Scans client files, generates manifests — no user input.
2. **Module load-time** (`ReactFlightWebpackNodeLoader.js`, `ReactFlightWebpackNodeRegister.js`): Transforms source files containing `"use client"` / `"use server"` directives — operates on source files from the file system, not user input.
3. **Runtime** (`server/*.js`, `client/*.js`): Thin wrappers re-exporting from `react-server/` and `react-client/`. The `decodeReply*`, `decodeAction`, `renderTo*Stream` functions accept user-controlled data but delegate to `resolveField`, `createResponse`, `getRoot` etc. in external packages.

No exploitable vulnerability exists within this codebase itself. However, one dependency concern is noted below.

---

## Checked and Cleared

| File:Line | Reason |
|---|---|
| `ReactFlightWebpackReferences.js:342-352` — `createClientModuleProxy` | Proxy handlers on client references. Property access comes from static JavaScript at build time (`clientModule.foo`), not user input. Proxy throws on unrecognized property access (line 190-194). |
| `ReactFlightWebpackReferences.js:201-307` — `getReference` | Walks `target[name]` on the target function object, but `name` is a static JS property access at module load time, not attacker-controlled. |
| `ReactFlightWebpackNodeLoader.js:25-108` — `_compile` override (Register) | Module compilation hook. Executes source files from disk. The `useServer` path (lines 80-106) tags exported functions with `registerServerReference` using file URLs as `$$id`. No user input flows here. |
| `ReactFlightWebpackNodeLoader.js:759-804` — ESM `load`/`transformSource` hooks | Transforms source code at module load time. Source comes from the file system. |
| `ReactFlightWebpackNodeLoader.js:509-569` — `parseExportNamesInto` | Recursively resolves `export *` statements. Source from file system. |
| `ReactFlightWebpackPlugin.js:66-525` — Webpack plugin | Build-time file scanning and manifest generation. Operates on project source files. |
| `server/ReactFlightDOMServerNode.js:554-678` — `decodeReply*` functions | Thin wrappers delegating to `react-server/src/ReactFlightReplyServer`. User-controlled `FormData` → `resolveField`/`resolveFileInfo` etc. The parsing/deserialization is in the external package. |
| `server/ReactFlightDOMServerEdge.js:247-311` — `decodeReply*` (Edge) | Same as Node variant — thin delegation. |
| `server/ReactFlightDOMServerBrowser.js:242-261` — `decodeReply` (Browser) | Same pattern — thin delegation. |
| `server/ReactFlightServerConfigWebpackBundler.js:89-101` — `getServerReference*` | Returns `$$id`/`$$bound` from a ServerReference object. The ID is a trusted build-time value. |
| `client/ReactFlightClientConfigBundlerNode.js:60-162` — `resolveClientReference`/`requireModule` | Dynamic `import(metadata.specifier)` at line 113. `specifier` comes from `ServerConsumerManifest` (trusted build artifact passed by the application). |
| `client/ReactFlightClientConfigBundlerWebpack.js:69-257` — `resolveClientReference`/`requireModule` | `__webpack_require__(metadata[ID])` at line 235. `ID` from trusted manifest. |
| `client/ReactFlightClientConfigBundlerWebpackBrowser.js:31-34` — `loadChunk` | `__webpack_chunk_load__(chunkId)` — chunk ID from trusted manifest. |
| `ReactFlightWebpackReferences.js:62-103` — `bind() | Server reference argument binding. `Function.prototype.bind` applied correctly. No eval or code execution. |
| `ReactFlightWebpackNodeLoader.js:668-691` — acorn parse | Parses transformed source at module load time. Source from file system, not user input. |
| `shared/ReactFlightImportMetadata.js` — data types | Pure type definitions and constants. No logic. |

## Dependencies

- **No lockfile present.** The codebase imports `acorn-loose@^8.3.0`, `neo-async@^2.6.1`, and `webpack-sources@^3.2.0` with no `package-lock.json` or `pnpm-lock.yaml`. No vulnerabilities detected at the ranges declared, but the absence of a lockfile means:
  - Exact transitive dependency versions are unknown.
  - `acorn-loose` → `acorn` chain: `acorn` has had ReDoS CVEs (GHSA-vh2m-22xx-3w3f). Ensure resolved `acorn` ≥ 8.14.0.
  - Peer dependency `webpack@^5.59.0` resolution is controlled by the consuming project.
- **`busboy`** is imported only as a Flow type (`import type {Busboy} from 'busboy'` in `server/ReactFlightDOMServerNode.js:17`). Not a runtime dependency of this package.
- **`module`** is a built-in Node.js module.

**Finding:** `acorn` transitive dependency may be below 8.14.0 with known ReDoS. **Severity: LOW** (only triggered at build time by source files the developer controls, not attacker input).

---

## LOW / INFORMATIONAL

### Missing lockfile prevents complete dependency audit

- **File:** (no file — project-level concern)
- **Impact:** Exact resolved versions of `acorn-loose` → `acorn` chain cannot be verified without a lockfile or monorepo-level manifest. If `acorn` < 8.14.0 is resolved, a ReDoS could be triggered during build-time source parsing. This is only exploitable by a developer with write access to the source tree.
- **Remediation:** Add the project to a monorepo with a lockfile, or run `npm install` / `pnpm install` in the parent to generate one, then re-scan.

---

## Remediation Summary

### Hardening (LOW)

1. **(project-level)** — Add a lockfile via `npm install` at the monorepo root to pin `acorn` ≥ 8.14.0 and audit the full transitive dependency tree.