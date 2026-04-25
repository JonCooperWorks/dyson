/* Dyson — per-chat session store.
 *
 * One reactive snapshot keyed by chatId; every session is a frozen value
 * produced through `updateSession(chatId, reducer)`.  The old app.jsx
 * held this as `sessionsRef.current = new Map<chatId, plainObject>` and
 * mutated fields in place then fired a global `bump()` — which silently
 * dropped renders when React's reconciler compared identical object
 * references and skipped the subtree.  The whole refactor is here to
 * stop that.
 *
 * A sibling non-reactive `resources` Map holds the chat's EventSource
 * and the per-chat ref counter.  These are resources, not data: they
 * don't belong in a snapshot and can't meaningfully be frozen anyway. */

import { createStore } from './createStore.js';

export function makeSession() {
  return {
    // Transcript blocks for the live turn stream.  Each entry:
    //   { role: 'user'|'agent', ts: string, blocks: Block[] }
    // Block discriminated union — see migration notes in turns.jsx.
    liveTurns: [],
    // Per-turn rating map: { [turnIndex]: emoji }.
    ratings: {},
    // Right-rail panel order — array of tool refs (strings) into app.tools.
    panels: [],
    openTool: null,
    openRating: null,
    running: false,
    phase: 'thinking',
    tname: '',
    liveToolRef: null,
    thinkingRef: null,
    loaded: false,
    justScrollOnNextRender: false,
    // Per-chat artefact list (populated from GET /artefacts + live SSE).
    artefacts: [],
    artefactsLoaded: false,
  };
}

export const sessions = createStore({});

export function getSession(chatId) {
  if (!chatId) return null;
  return sessions.getSnapshot()[chatId] || null;
}

// Guarantees a session exists for the chatId after the call returns.
// Idempotent — if one is already in the store, the reducer returns the
// same state reference and the dispatch is a no-op.
export function ensureSession(chatId) {
  if (!chatId) return null;
  sessions.dispatch(state => state[chatId] ? state : { ...state, [chatId]: makeSession() });
  return getSession(chatId);
}

// Produces the next session via `reducer(previous)`.  Returning the same
// reference from the reducer is the supported way to signal no-op; the
// outer dispatch then short-circuits too.
export function updateSession(chatId, reducer) {
  if (!chatId) return;
  sessions.dispatch(state => {
    const prev = state[chatId] || makeSession();
    const next = reducer(prev);
    if (next === prev) return state;
    return { ...state, [chatId]: next };
  });
}

export function deleteSession(chatId) {
  sessions.dispatch(state => {
    if (!(chatId in state)) return state;
    const next = { ...state };
    delete next[chatId];
    return next;
  });
  disposeResources(chatId);
}

// -- Non-reactive resources: EventSource + per-chat counter --------------

const resources = new Map();

export function getResources(chatId) {
  let r = resources.get(chatId);
  if (!r) { r = { es: null, counter: 0 }; resources.set(chatId, r); }
  return r;
}

// Generates the next chat-id-prefixed tool ref.  Prefix prevents two
// simultaneously-streaming chats from colliding in app.tools.
export function mintToolRef(chatId, kind) {
  const r = getResources(chatId);
  r.counter += 1;
  return `${chatId}-${kind}-${r.counter}`;
}

function disposeResources(chatId) {
  const r = resources.get(chatId);
  if (!r) return;
  if (r.es) { try { r.es.close(); } catch { /* closed already */ } }
  resources.delete(chatId);
}

// Test hook — clears both the reactive map and the resources map.  Never
// call from production code.
export function __resetSessionsForTests() {
  sessions.dispatch(() => ({}));
  for (const chatId of [...resources.keys()]) disposeResources(chatId);
}

// -- Pure session reducers ---------------------------------------------
// Reused across streamCallbacks, /clear, and hydration.  Each returns a
// new session snapshot, or the same reference to signal no-op.  Kept
// out of the React tree so they can be unit-tested and so streamCallbacks
// can compose them without prop-drilling.

export const mapLastTurn = (s, fn) => {
  if (!s.liveTurns.length) return s;
  const i = s.liveTurns.length - 1;
  const next = fn(s.liveTurns[i]);
  if (next === s.liveTurns[i]) return s;
  return { ...s, liveTurns: [...s.liveTurns.slice(0, i), next] };
};

export const appendBlock = (s, block) =>
  mapLastTurn(s, t => ({ ...t, blocks: [...t.blocks, block] }));

// Walk from the end to find the most recent agent turn.  Returns -1
// when no agent turn exists.  Used by the SSE delta handlers so a
// queued user message sitting at the tail (no agent placeholder yet,
// because we don't push one when sending while running) doesn't
// receive deltas that belong to the in-flight agent turn earlier in
// the array.
export const lastAgentIndex = (s) => {
  for (let i = s.liveTurns.length - 1; i >= 0; i--) {
    if (s.liveTurns[i].role === 'agent') return i;
  }
  return -1;
};

// Apply `fn` to the agent turn that should absorb the next delta.
// Forces a fresh agent turn when `s.nextAgentNew` is set (we just
// emitted Done for the previous turn and the next delta belongs to
// a new run — the queue-drain case) OR when no agent turn exists yet.
// Otherwise mutates the most recent agent turn in place, skipping
// past any user turns at the tail.
export const mapAgentTail = (s, fn) => {
  if (s.nextAgentNew) {
    // Fresh turn after a Done — flip running back on so the typing
    // indicator returns while the queue-drain reply streams in.
    const fresh = fn({ role: 'agent', ts: '', blocks: [] });
    return {
      ...s,
      nextAgentNew: false,
      running: true,
      phase: 'thinking',
      thinkingRef: null,
      liveTurns: [...s.liveTurns, fresh],
    };
  }
  const i = lastAgentIndex(s);
  if (i < 0) {
    return {
      ...s,
      running: true,
      phase: 'thinking',
      thinkingRef: null,
      liveTurns: [...s.liveTurns, fn({ role: 'agent', ts: '', blocks: [] })],
    };
  }
  const next = fn(s.liveTurns[i]);
  if (next === s.liveTurns[i]) return s;
  return {
    ...s,
    liveTurns: [...s.liveTurns.slice(0, i), next, ...s.liveTurns.slice(i + 1)],
  };
};

export const appendAgentBlock = (s, block) =>
  mapAgentTail(s, t => ({ ...t, blocks: [...t.blocks, block] }));

export const openPanel = (s, ref) => ({
  ...s,
  panels: s.panels.includes(ref) ? s.panels : [...s.panels, ref],
  openTool: ref,
});

export const closePanel = (s, ref) => s.panels.includes(ref)
  ? { ...s, panels: s.panels.filter(x => x !== ref), openTool: s.openTool === ref ? null : s.openTool }
  : s;
