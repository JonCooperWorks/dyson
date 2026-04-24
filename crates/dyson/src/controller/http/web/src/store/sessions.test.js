import { describe, it, expect, beforeEach } from 'vitest';
import {
  sessions,
  getSession,
  ensureSession,
  updateSession,
  deleteSession,
  makeSession,
  mintToolRef,
  getResources,
  __resetSessionsForTests,
} from './sessions.js';

beforeEach(() => {
  __resetSessionsForTests();
});

describe('sessions store — ensure + update isolation', () => {
  it('ensureSession creates a fresh session and is idempotent', () => {
    ensureSession('c1');
    const a = getSession('c1');
    expect(a).not.toBeNull();
    expect(a.liveTurns).toEqual([]);
    const before = sessions.getSnapshot();
    ensureSession('c1');
    // Second call returns the same snapshot reference — dispatch was a no-op.
    expect(sessions.getSnapshot()).toBe(before);
  });

  it('updateSession produces a fresh frozen session when the reducer returns new state', () => {
    ensureSession('c1');
    const before = getSession('c1');
    updateSession('c1', s => ({ ...s, running: true }));
    const after = getSession('c1');
    expect(after).not.toBe(before);
    expect(after.running).toBe(true);
    expect(Object.isFrozen(after)).toBe(true);
  });

  it('updateSession is a no-op when the reducer returns the same reference', () => {
    ensureSession('c1');
    const before = sessions.getSnapshot();
    updateSession('c1', s => s);
    expect(sessions.getSnapshot()).toBe(before);
  });

  it('sessions are isolated across chatIds', () => {
    updateSession('c1', s => ({ ...s, running: true, tname: 'bash' }));
    updateSession('c2', s => ({ ...s, tname: 'diff' }));
    expect(getSession('c1').running).toBe(true);
    expect(getSession('c1').tname).toBe('bash');
    expect(getSession('c2').running).toBe(false);
    expect(getSession('c2').tname).toBe('diff');
  });

  it('deleteSession removes the session from the snapshot', () => {
    ensureSession('c1');
    ensureSession('c2');
    deleteSession('c1');
    expect(getSession('c1')).toBeNull();
    expect(getSession('c2')).not.toBeNull();
  });

  it('null / empty chatId is a safe no-op', () => {
    const before = sessions.getSnapshot();
    ensureSession(null);
    updateSession(null, s => ({ ...s, running: true }));
    expect(sessions.getSnapshot()).toBe(before);
  });

  it('snapshot entries cannot be mutated in place', () => {
    ensureSession('c1');
    const snap = sessions.getSnapshot();
    expect(() => { snap['c1'].running = true; }).toThrow();
    expect(() => { snap['c1'].panels.push('x'); }).toThrow();
  });
});

describe('sessions store — non-reactive resources', () => {
  it('getResources returns a stable record per chat', () => {
    const a = getResources('c1');
    const b = getResources('c1');
    expect(a).toBe(b);
    expect(a.counter).toBe(0);
    expect(a.es).toBeNull();
  });

  it('mintToolRef prefixes with the chat id and bumps the counter', () => {
    const r1 = mintToolRef('c1', 'live');
    const r2 = mintToolRef('c1', 'live');
    const other = mintToolRef('c2', 'thinking');
    expect(r1).toBe('c1-live-1');
    expect(r2).toBe('c1-live-2');
    expect(other).toBe('c2-thinking-1');
  });
});

describe('makeSession shape', () => {
  it('makeSession returns a fresh object with the expected defaults', () => {
    const s = makeSession();
    expect(s.liveTurns).toEqual([]);
    expect(s.panels).toEqual([]);
    expect(s.running).toBe(false);
    expect(s.loaded).toBe(false);
    expect(s.artefacts).toEqual([]);
  });
});
