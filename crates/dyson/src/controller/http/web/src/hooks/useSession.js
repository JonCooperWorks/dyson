/* Dyson — React hooks for the per-chat session store.
 *
 * useSession(chatId) returns the current frozen session for that chat,
 * or null if none has been created yet (caller triggers ensureSession /
 * updateSession to make one).  The hook re-renders only when that chat's
 * slice actually changes — sibling chats streaming in the background
 * don't churn the active view.
 */

import { useSyncExternalStore, useRef, useCallback } from 'react';
import { sessions, updateSession } from '../store/sessions.js';

export function useSession(chatId) {
  const cacheRef = useRef(null);

  const getSnapshot = () => {
    const snap = sessions.getSnapshot();
    const cache = cacheRef.current;
    const current = chatId ? (snap[chatId] || null) : null;
    if (cache && cache.chatId === chatId && cache.value === current) return cache.value;
    cacheRef.current = { chatId, value: current };
    return current;
  };

  return useSyncExternalStore(sessions.subscribe, getSnapshot, getSnapshot);
}

// Returns a stable dispatcher bound to the chatId, so callers can pass
// `const mutate = useSessionMutator(conv)` into handlers without having
// to thread the chat id through every reducer invocation.
export function useSessionMutator(chatId) {
  return useCallback((reducer) => updateSession(chatId, reducer), [chatId]);
}
