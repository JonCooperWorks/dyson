# Web SPA Components (`web/src/components`)

## Overview
React (JSX) view layer for the Dyson web UI: the root `App` shell, conversation turns and composer, tool panels, secondary views (Mind, Activity, Audit, Artefacts), the MCP elicitation modal, the command palette, and small shared widgets.

## Interface
- `App`, `streamCallbacks`, `attachLiveStream`, `keyboardInsetForVisualViewport` [app.jsx:App]
- `parseHash(rawArg)` / `buildHash(state)` - URL hash routing helpers [app.jsx:parseHash, app.jsx:buildHash]
- `Turn`, `ThinkingBlock`, `ToolChip`, `ToolBlock`, `FileBlock`, `ArtefactBlock`, `ErrorBlock`, `RunStatusStrip`, `TypingIndicator`, `Composer`, `EmptyState`, `markdown`, `prettySize` [turns.jsx:Turn]
- `PanelChrome`, `BashPanel`, `DiffPanel`, `SbomPanel`, `TaintPanel`, `ThinkingPanel`, `ImagePanel`, `FallbackPanel`, `ReadPanel`, `SubagentPanel`, `SecurityHarnessPanel`, and further tool panels [panels.jsx:PanelChrome]
- `MindView`, `ActivityView`, `AuditView`, `ArtefactsView`, `ArtefactReader` [views-secondary.jsx:MindView]
- `TopBar`, `LeftRail`, `NAVS`, `providerPresentation`, `brandLabel` [views.jsx:TopBar]
- `ElicitationModal` - renders pending MCP elicitation requests [ElicitationModal.jsx:ElicitationModal]
- `CommandPalette`, `buildCommandItems` [command-palette.jsx:CommandPalette]
- `RunControls` [run-controls.jsx:RunControls], `ShareMenu` [share-menu.jsx:ShareMenu], `Icon`, `Chip`, `Kbd` [icons.jsx:Icon]

## Invariants
- Components read shared state through `useAppState` selectors (e.g. `s.skills?.mcp`) rather than globals [app.jsx:App, turns.jsx:Composer]

## Side Effects
- Fetches artefact bodies from `/api/conversations/:chatId/artefacts/:id` [turns.jsx:ArtefactBlock]
- POSTs `{artefact_id, chat_id, ttl}` to the Swarm-hosted `/_swarm/share-mint` endpoint to mint share links [turns.jsx:ArtefactShareControl, views-secondary.jsx:ArtefactRowShareControl, views-secondary.jsx:ArtefactReader]
- Resolves orphan image artefact URLs out-of-band with `fetch(url)` while hydrating a transcript, then `updateTool` [app.jsx:hydrateTranscript]

## Verification
- Run: `npm test` (in `dyson/crates/dyson/src/controller/http/web`)
