# Web SPA Source (`web/src`)

## Overview
Source root of the Dyson web UI (React 19 + Vite). `main.jsx` is the Vite entry point: it imports the stylesheets, initialises the theme controller, runs auth bootstrap, constructs the single `DysonClient`, starts the cold-load/poll loop, and mounts `<App/>` and `<ElicitationModal/>` inside `<ApiProvider>`.

## Interface
- `main.jsx` (entry; no exports) - `bootstrapAuth().then(session => new DysonClient({ getToken: session.getToken }) ...)` [main.jsx:bootstrapAuth]
- [api](./api/EGM.md) - auth bootstrap, HTTP + SSE client, SSE parsing, boot/poll loop
- [components](./components/EGM.md) - React views, panels, composer, modals
- [hooks](./hooks/EGM.md) - `useApi`, `useAppState`, `useSession`
- [store](./store/EGM.md) - app-wide and per-chat reactive stores
- `styles/` - CSS (not documented); `__tests__/` - test-only

## Invariants
- Auth bootstrap runs before any rendering so an OIDC redirect happens before paint [main.jsx:bootstrapAuth]
- No window globals: the client is injected through `ApiProvider` [main.jsx:ApiProvider]

## Side Effects
- Initialises theme with `createThemeController({ storageKey: 'dyson-theme' })` from `dyson-common-ui` [main.jsx:createThemeController]
- On auth bootstrap failure, writes a "Sign-in failed" frame into `#root` and logs to console [main.jsx:bootstrapAuth]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
