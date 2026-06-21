/* Tests for useEscapeKey — the hook behind every modal/overlay's
 * Escape-to-dismiss (shared <Modal>, ModelMenu, drawer scrims, etc.). */
import { describe, expect, test, afterEach, vi } from 'vitest';
import { renderHook, cleanup, fireEvent } from '@testing-library/react';

import { useEscapeKey } from './useEscapeKey.js';

afterEach(cleanup);

describe('useEscapeKey', () => {
  test('calls the handler on Escape', () => {
    const fn = vi.fn();
    renderHook(() => useEscapeKey(fn));
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(fn).toHaveBeenCalledTimes(1);
  });

  test('ignores other keys', () => {
    const fn = vi.fn();
    renderHook(() => useEscapeKey(fn));
    fireEvent.keyDown(window, { key: 'Enter' });
    fireEvent.keyDown(window, { key: 'a' });
    expect(fn).not.toHaveBeenCalled();
  });

  test('a falsy handler attaches no listener and does not throw', () => {
    renderHook(() => useEscapeKey(null));
    expect(() => fireEvent.keyDown(window, { key: 'Escape' })).not.toThrow();
  });

  test('removes the listener on unmount', () => {
    const fn = vi.fn();
    const { unmount } = renderHook(() => useEscapeKey(fn));
    unmount();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(fn).not.toHaveBeenCalled();
  });
});
