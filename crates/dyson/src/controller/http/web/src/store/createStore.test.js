import { describe, it, expect, vi } from 'vitest';
import { createStore, deepFreeze } from './createStore.js';

describe('deepFreeze', () => {
  it('freezes nested objects in place', () => {
    const obj = { a: { b: { c: 1 } }, arr: [{ x: 1 }] };
    deepFreeze(obj);
    expect(Object.isFrozen(obj)).toBe(true);
    expect(Object.isFrozen(obj.a)).toBe(true);
    expect(Object.isFrozen(obj.a.b)).toBe(true);
    expect(Object.isFrozen(obj.arr)).toBe(true);
    expect(Object.isFrozen(obj.arr[0])).toBe(true);
  });

  it('is idempotent on already-frozen subtrees', () => {
    const shared = Object.freeze({ k: 1 });
    const obj = { inner: shared };
    expect(() => deepFreeze(obj)).not.toThrow();
    expect(Object.isFrozen(obj)).toBe(true);
  });

  it('tolerates null and primitive values', () => {
    expect(deepFreeze(null)).toBe(null);
    expect(deepFreeze(42)).toBe(42);
    expect(deepFreeze('x')).toBe('x');
  });
});

describe('createStore', () => {
  it('exposes an initial frozen snapshot', () => {
    const store = createStore({ n: 1 });
    const snap = store.getSnapshot();
    expect(snap).toEqual({ n: 1 });
    expect(Object.isFrozen(snap)).toBe(true);
    expect(() => { snap.n = 2; }).toThrow();
  });

  it('dispatch installs the reducer result and notifies subscribers', () => {
    const store = createStore({ n: 1 });
    const fn = vi.fn();
    store.subscribe(fn);
    store.dispatch(s => ({ ...s, n: s.n + 1 }));
    expect(fn).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot()).toEqual({ n: 2 });
  });

  it('dispatch is a no-op when the reducer returns the same reference', () => {
    const store = createStore({ n: 1 });
    const fn = vi.fn();
    store.subscribe(fn);
    const before = store.getSnapshot();
    store.dispatch(s => s);
    expect(fn).not.toHaveBeenCalled();
    expect(store.getSnapshot()).toBe(before);
  });

  it('unsubscribe stops the listener from firing', () => {
    const store = createStore({ n: 1 });
    const fn = vi.fn();
    const unsub = store.subscribe(fn);
    unsub();
    store.dispatch(s => ({ ...s, n: 2 }));
    expect(fn).not.toHaveBeenCalled();
  });

  it('multiple subscribers all fire on a single dispatch', () => {
    const store = createStore({ n: 1 });
    const a = vi.fn();
    const b = vi.fn();
    store.subscribe(a);
    store.subscribe(b);
    store.dispatch(s => ({ ...s, n: s.n + 1 }));
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(1);
  });

  it('snapshot after dispatch is itself frozen', () => {
    const store = createStore({ nested: { v: 1 } });
    store.dispatch(s => ({ ...s, nested: { v: 2 } }));
    const snap = store.getSnapshot();
    expect(Object.isFrozen(snap)).toBe(true);
    expect(Object.isFrozen(snap.nested)).toBe(true);
    expect(() => { snap.nested.v = 99; }).toThrow();
  });
});
