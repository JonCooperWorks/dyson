/* Dyson — React hook for the app store.
 *
 * Thin binding over dyson-common-ui's createUseAppState: the shared hook
 * carries the selector-identity cache (it originated in this app), so
 * subscribers still only re-render when their slice changes by value.
 * Kept at this path so every importer's `../hooks/useAppState.js` stays put.
 */

import { createUseAppState } from 'dyson-common-ui';
import { app } from '../store/app.js';

export const useAppState = createUseAppState(app);
