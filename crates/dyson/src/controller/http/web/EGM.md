# Dyson Web UI Package (`controller/http/web`)

## Overview
The `dyson-web` npm package: the single-page web UI served by the Rust HTTP controller. Vite builds it into `dist/`, which `crates/dyson/build.rs` embeds into the binary. Local direct files are the build config, the HTML shell, and a dev-only backend mock.

## Interface
- [src](./src/EGM.md) - SPA source (entry, api, components, hooks, store)
- `package.json` scripts: `dev` (`vite`), `build` (`lint && vitest run && vite build`), `build:nocheck`, `test` (`vitest run`), `test:watch`, `lint` (`oxlint -c .oxlintrc.json --deny-warnings src`) [package.json:scripts]
- Dependencies: `dyson-common-ui` (git tag `ui-v0.1.15`), `markdown-it`, `react`, `react-dom` [package.json:dependencies]
- `defineConfig` with `inlineCss()` plugin, React plugin, jsdom test env, dev server, build options [vite.config.js:defineConfig, vite.config.js:inlineCss]
- `dysonMock()` - dev-server mock of the controller API, enabled only with `VITE_DYSON_MOCK=1` [devmock.js:dysonMock, vite.config.js:useMock]
- `index.html` - SPA shell with an inline pre-paint theme script [index.html]

## Invariants
- Node `>=20` [package.json:engines]
- Production builds emit to `dist/` with `emptyOutDir: true` and deterministic `assets/[name]-[hash]` filenames, which `build.rs` walks to generate the Rust asset table [vite.config.js:defineConfig]
- `cssTarget: 'safari14'` keeps legacy `@media (max-width: ...)` syntax for older iOS devices [vite.config.js:defineConfig]
- `inlineCss` applies only at build and replaces CSS `<link>` tags with inline `<style>` [vite.config.js:inlineCss]
- The mock plugin is never active in production builds [vite.config.js:useMock]

## Side Effects
- Dev server on port 5173 proxies `/api` and `/artefacts` to `http://127.0.0.1:7878` [vite.config.js:defineConfig]
- `index.html` reads `dyson-theme` from `localStorage` to set `data-theme` before paint [index.html]
- Build deletes and rewrites `dist/` [vite.config.js:defineConfig]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
