/* Dyson — minimal subscribe / getSnapshot / dispatch store.
 *
 * Built for React's `useSyncExternalStore`: one frozen snapshot, listeners
 * fire synchronously on change, reducers that return the same reference
 * short-circuit (no allocation, no notification).  Every snapshot is
 * deep-frozen — the whole refactor exists to kill the mutate-then-bump()
 * pattern that was silently dropping renders, and a frozen snapshot is
 * how we enforce the contract at runtime.
 */

export function deepFreeze(obj) {
  if (obj === null || typeof obj !== 'object') return obj;
  if (Object.isFrozen(obj)) return obj;
  Object.freeze(obj);
  for (const key of Object.keys(obj)) {
    const v = obj[key];
    if (v !== null && typeof v === 'object' && !Object.isFrozen(v)) deepFreeze(v);
  }
  return obj;
}

export function createStore(initial) {
  let snapshot = deepFreeze(initial);
  const listeners = new Set();

  const getSnapshot = () => snapshot;

  const subscribe = (fn) => {
    listeners.add(fn);
    return () => { listeners.delete(fn); };
  };

  // Reducer contract: return the SAME reference to signal "no change" —
  // cheaper than a deep equality check and it surfaces bugs where the
  // caller thought they were mutating in place.
  const dispatch = (reducer) => {
    const next = reducer(snapshot);
    if (next === snapshot) return;
    snapshot = deepFreeze(next);
    for (const fn of listeners) fn();
  };

  return { getSnapshot, subscribe, dispatch };
}
