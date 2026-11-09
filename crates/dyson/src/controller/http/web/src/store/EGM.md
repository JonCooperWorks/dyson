# Web SPA Stores (`web/src/store`)

## Overview
Reactive client-side state built on `createStore` from `dyson-common-ui`. `app.js` holds cross-chat state (live flag, conversations, providers, commands, mind, skills, activity, tool views); `sessions.js` holds frozen per-chat session snapshots plus a non-reactive per-chat resource map; `constants.js` holds fallback slash commands.

## Interface
- `app` store and mutators: `setLive`, `setConversations`, `upsertConversation`, `removeConversation`, `markConversationHasArtefacts`, `setProviders`, `setCommands`, `switchProviderModel`, `setMind`, `setAgentName`, `mergeMcpProbes`, `setSkills`, `setAgentInfo`, `setActivity`, `setTool`, `mergeTools`, `updateTool`, `requestOpenArtefact`, `clearPendingArtefact`, `requestToggleArtefactsDrawer` [app.js:app]
- `sessions` store and accessors: `makeSession`, `getSession`, `ensureSession`, `updateSession`, `deleteSession`, `getResources`, `mintToolRef` [sessions.js:sessions]
- Session reducers: `mapLastTurn`, `appendBlock`, `lastAgentIndex`, `mapAgentTail`, `appendAgentBlock`, `settleRun`, `pushUserMessage`, `admitUserMessage`, `openPanel`, `closePanel`, `closeAllPanels`, `setComposerDraft`, `clearComposerDraft`, `setQueueMode`, `setNextRunModel` [sessions.js:pushUserMessage]
- `FALLBACK_SLASH_COMMANDS` / `SLASH_COMMANDS` [constants.js:FALLBACK_SLASH_COMMANDS]
- Test-only resets: `__resetAppStoreForTests`, `__resetSessionsForTests` [app.js:__resetAppStoreForTests, sessions.js:__resetSessionsForTests]

## Invariants
- Sessions are immutable values replaced through `updateSession(chatId, reducer)`; never mutate in place [sessions.js:updateSession]
- EventSources and per-chat ref counters live in the non-reactive `resources` map, not in snapshots [sessions.js:getResources]
- Skill payloads are normalised so `builtin`, `mcp`, `denials` are always arrays [app.js:normalizeSkills]
- `FALLBACK_SLASH_COMMANDS` is frozen; the live list comes from `/api/commands` [constants.js:FALLBACK_SLASH_COMMANDS]

## Side Effects
- In-memory state only; no network or storage access [app.js:app, sessions.js:sessions]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
