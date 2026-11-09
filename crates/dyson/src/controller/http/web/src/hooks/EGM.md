# Web SPA Hooks (`web/src/hooks`)

## Overview
React hooks binding components to the API client context and the reactive stores in `../store`.

## Interface
- `ApiProvider({ client, children })` - React context provider for the single `DysonClient` [useApi.js:ApiProvider]
- `useApi()` - returns the client; throws if no provider is mounted [useApi.js:useApi]
- `useApiOptional()` - non-throwing variant returning `null` [useApi.js:useApiOptional]
- `useAppState(selector)` - app-store selector hook built with `createUseAppState(app)` from `dyson-common-ui` [useAppState.js:useAppState]
- `useSession(chatId)` - per-chat session snapshot via `useSyncExternalStore` [useSession.js:useSession]
- `useSessionMutator(chatId)` - stable dispatcher wrapping `updateSession(chatId, reducer)` [useSession.js:useSessionMutator]

## Invariants
- `useApi` throws `useApi: no ApiContext provider (wrap in <ApiProvider>)` outside a provider [useApi.js:useApi]
- `useSession` returns a cached value per `chatId` so re-renders happen only when that chat's slice changes; returns `null` for unknown chats [useSession.js:useSession]

## Side Effects
- None (dispatches go through `updateSession` in the store) [useSession.js:useSessionMutator]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
