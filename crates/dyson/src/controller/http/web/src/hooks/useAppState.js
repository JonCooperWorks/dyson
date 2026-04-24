/* Dyson — React hook for the app store.
 *
 * Wraps useSyncExternalStore with a cached selector so subscribers only
 * re-render when their slice actually changes by value.  The cache is
 * tolerant of React's double-invoke-in-strict-mode: the ref closure holds
 * the last snapshot reference, and identity equality on the full snapshot
 * short-circuits before the selector runs.
 */

import { useSyncExternalStore, useRef } from 'react';
import { app } from '../store/app.js';

const identity = (s) => s;

export function useAppState(selector) {
  const sel = selector || identity;
  const cacheRef = useRef(null);

  // getSnapshot is called during render; the cache holds the last
  // (fullSnapshot, selectedValue) pair so repeated calls for the same
  // store state return the same selected reference — that's what lets
  // React skip the re-render when the selected slice hasn't changed.
  const getSnapshot = () => {
    const snap = app.getSnapshot();
    const cache = cacheRef.current;
    if (cache && cache.snap === snap) return cache.selected;
    const selected = sel(snap);
    if (cache && Object.is(cache.selected, selected)) {
      cacheRef.current = { snap, selected: cache.selected };
      return cache.selected;
    }
    cacheRef.current = { snap, selected };
    return selected;
  };

  return useSyncExternalStore(app.subscribe, getSnapshot, getSnapshot);
}
