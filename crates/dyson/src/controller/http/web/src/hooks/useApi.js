/* Dyson — React context for the HTTP + SSE client.
 *
 * main.jsx constructs a single DysonClient and wraps the tree in
 * <ApiProvider client={...}>.  Components pull it via `useApi()`.
 * Tests mount their own provider with a mock client — replacing the old
 * `window.DysonLive` handshake with a normal React injection surface.
 */

import React, { createContext, useContext } from 'react';

const ApiContext = createContext(null);

export function ApiProvider({ client, children }) {
  return React.createElement(ApiContext.Provider, { value: client }, children);
}

export function useApi() {
  const c = useContext(ApiContext);
  if (!c) throw new Error('useApi: no ApiContext provider (wrap in <ApiProvider>)');
  return c;
}

// Non-throwing variant — returns null if no provider is mounted.
// Use when a component should degrade gracefully (e.g. Composer's
// checkpoint-upload affordance is a nice-to-have, not load-bearing
// for the composer's core text-entry behavior).  Tests can mount the
// component without an ApiProvider when they only exercise unrelated
// branches.
export function useApiOptional() {
  return useContext(ApiContext);
}
