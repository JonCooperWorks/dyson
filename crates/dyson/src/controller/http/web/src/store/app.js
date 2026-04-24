/* Dyson — app-wide reactive state.
 *
 * Holds everything that's shared across chats: the live-mode flag, the
 * active model, the conversation sidebar list, providers, activity, the
 * mind file index, and the tool view dictionary.  Per-chat session data
 * lives in sessions.js — this store is the minimum needed to paint the
 * chrome (TopBar / LeftRail / Mind / Activity) and to serve tool panels
 * which are logically global (keyed by chatId-prefixed ref). */

import { createStore } from './createStore.js';

const INITIAL = {
  // true once /api/conversations has responded.  Cold mode renders the
  // shell with empty lists until then.
  live: false,
  activeModel: '',
  // Flat list — a chat carries `source: 'http' | 'telegram' | 'swarm'`
  // if we ever want to render a provenance badge.  The old DYSON_DATA
  // shape grouped by controller but every consumer flattened it back,
  // so the group object was pure ceremony.
  conversations: [],
  providers: [],
  activity: [],
  mind: { backend: '', files: [], open: { path: '', content: '' } },
  // Tool views by ref.  Refs are chat-id-prefixed on mint (`${chatId}-live-N`)
  // so two chats can stream simultaneously without colliding — see
  // sessions.js `mintToolRef` for the generator.
  tools: {},
  skills: { builtin: [], mcp: [], denials: [] },
  // Imperative UI pokes.  Replaces the old `window.dispatchEvent(new
  // CustomEvent(...))` channel: a counter is the cheap way to signal
  // "fire this once" without the listener missing the dispatch (React
  // props propagate whenever the value changes).  Readers compare the
  // nonce against the last value they processed.
  ui: {
    pendingArtefactId: null,   // deep-link / chip click → ArtefactsView picks it up
    openRailNonce: 0,          // bumped to force the right rail open
    toggleArtefactsDrawerNonce: 0,
  },
};

export const app = createStore(INITIAL);

// Actions — the only supported way to mutate the store.  Each returns
// void and dispatches the minimum-viable reducer.  Selectors are inline
// lambdas at the call site (`useAppState(s => s.foo)`) — the hook caches
// the selected slice by identity so recreating the selector per render
// doesn't cost anything.

export function setLive(v) {
  app.dispatch(s => s.live === v ? s : { ...s, live: v });
}

export function setConversations(list) {
  app.dispatch(s => ({ ...s, conversations: list }));
}

export function upsertConversation(row) {
  app.dispatch(s => {
    const idx = s.conversations.findIndex(c => c.id === row.id);
    if (idx === -1) return { ...s, conversations: [row, ...s.conversations] };
    const next = s.conversations.slice();
    next[idx] = { ...next[idx], ...row };
    return { ...s, conversations: next };
  });
}

export function removeConversation(id) {
  app.dispatch(s => {
    const idx = s.conversations.findIndex(c => c.id === id);
    if (idx === -1) return s;
    const next = s.conversations.slice();
    next.splice(idx, 1);
    return { ...s, conversations: next };
  });
}

export function markConversationHasArtefacts(id) {
  app.dispatch(s => {
    const idx = s.conversations.findIndex(c => c.id === id);
    if (idx === -1 || s.conversations[idx].hasArtefacts) return s;
    const next = s.conversations.slice();
    next[idx] = { ...next[idx], hasArtefacts: true };
    return { ...s, conversations: next };
  });
}

export function setProviders(providers, activeModel) {
  app.dispatch(s => ({ ...s, providers, activeModel }));
}

export function switchProviderModel(provider, modelName) {
  app.dispatch(s => ({
    ...s,
    activeModel: modelName,
    providers: s.providers.map(p => ({
      ...p,
      active: p.id === provider,
      activeModel: p.id === provider ? modelName : p.activeModel,
    })),
  }));
}

export function setMind(mind) {
  app.dispatch(s => ({ ...s, mind: { ...s.mind, ...mind } }));
}

export function setActivity(lanes) {
  app.dispatch(s => ({ ...s, activity: lanes }));
}

// Tool views — streaming touches these per-delta.  Every mutation produces
// a new `tools` dict; the old tool object is discarded.  Deep-freeze in
// createStore ensures any accidental `tools[id].foo = bar` throws.
export function setTool(ref, tool) {
  app.dispatch(s => ({ ...s, tools: { ...s.tools, [ref]: tool } }));
}

// Merge many tools at once — cheaper than one dispatch per entry when
// hydrating a transcript on chat reload.
export function mergeTools(patch) {
  app.dispatch(s => ({ ...s, tools: { ...s.tools, ...patch } }));
}

// `patchOrReducer` can be a shallow patch object or a reducer function —
// the function form lets SSE callbacks rewrite deep fields (e.g. body.text
// on a streaming tool) without a read-compute-write round-trip in the
// caller.  Returning the same reference from the reducer short-circuits.
export function updateTool(ref, patchOrReducer) {
  app.dispatch(s => {
    const existing = s.tools[ref];
    if (!existing) return s;
    const next = typeof patchOrReducer === 'function'
      ? patchOrReducer(existing)
      : { ...existing, ...patchOrReducer };
    if (next === existing) return s;
    return { ...s, tools: { ...s.tools, [ref]: next } };
  });
}

// UI pokes — bump a nonce (or set a value) so subscribers can key their
// useEffect off the change.  pendingArtefactId is a value because the
// payload matters; the others are nonces because the signal is binary.
export function requestOpenArtefact(id) {
  app.dispatch(s => ({ ...s, ui: { ...s.ui, pendingArtefactId: id } }));
}

export function clearPendingArtefact() {
  app.dispatch(s => s.ui.pendingArtefactId == null
    ? s
    : { ...s, ui: { ...s.ui, pendingArtefactId: null } });
}

export function requestOpenRail() {
  app.dispatch(s => ({ ...s, ui: { ...s.ui, openRailNonce: s.ui.openRailNonce + 1 } }));
}

export function requestToggleArtefactsDrawer() {
  app.dispatch(s => ({
    ...s,
    ui: { ...s.ui, toggleArtefactsDrawerNonce: s.ui.toggleArtefactsDrawerNonce + 1 },
  }));
}

// Test hook — resets the store between tests without exposing the
// internal snapshot reference.  Never call from production code.
export function __resetAppStoreForTests() {
  app.dispatch(() => INITIAL);
}
