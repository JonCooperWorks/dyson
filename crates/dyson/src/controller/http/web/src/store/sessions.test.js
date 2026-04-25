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
  mapLastTurn,
  appendBlock,
  mapAgentTail,
  appendAgentBlock,
  lastAgentIndex,
  openPanel,
  closePanel,
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

describe('pure session reducers', () => {
  const turn = (blocks = []) => ({ role: 'agent', ts: '', blocks });

  it('mapLastTurn returns the same state when there are no turns', () => {
    const s = makeSession();
    expect(mapLastTurn(s, t => ({ ...t, blocks: ['x'] }))).toBe(s);
  });

  it('mapLastTurn returns the same state when the reducer is a no-op', () => {
    const s = { ...makeSession(), liveTurns: [turn()] };
    expect(mapLastTurn(s, t => t)).toBe(s);
  });

  it('mapLastTurn only replaces the last turn', () => {
    const first = turn([{ type: 'text', text: 'a' }]);
    const last = turn([{ type: 'text', text: 'b' }]);
    const s = { ...makeSession(), liveTurns: [first, last] };
    const next = mapLastTurn(s, t => ({ ...t, blocks: [] }));
    expect(next.liveTurns[0]).toBe(first);
    expect(next.liveTurns[1]).not.toBe(last);
    expect(next.liveTurns[1].blocks).toEqual([]);
  });

  it('appendBlock pushes onto the last turn', () => {
    const s = { ...makeSession(), liveTurns: [turn([{ type: 'text', text: 'hi' }])] };
    const next = appendBlock(s, { type: 'text', text: ' there' });
    expect(next.liveTurns[0].blocks).toHaveLength(2);
    expect(next.liveTurns[0].blocks[1]).toEqual({ type: 'text', text: ' there' });
  });

  it('openPanel adds to panels and sets openTool', () => {
    const s = makeSession();
    const next = openPanel(s, 'r1');
    expect(next.panels).toEqual(['r1']);
    expect(next.openTool).toBe('r1');
  });

  it('openPanel does not duplicate panels', () => {
    const s = { ...makeSession(), panels: ['r1'] };
    const next = openPanel(s, 'r1');
    expect(next.panels).toEqual(['r1']);
    expect(next.openTool).toBe('r1');
  });

  it('closePanel removes the ref and clears openTool when it was the active one', () => {
    const s = { ...makeSession(), panels: ['r1', 'r2'], openTool: 'r1' };
    const next = closePanel(s, 'r1');
    expect(next.panels).toEqual(['r2']);
    expect(next.openTool).toBeNull();
  });

  it('closePanel leaves openTool untouched when closing a sibling ref', () => {
    const s = { ...makeSession(), panels: ['r1', 'r2'], openTool: 'r2' };
    const next = closePanel(s, 'r1');
    expect(next.openTool).toBe('r2');
  });

  it('closePanel is a no-op when the ref isn\'t open', () => {
    const s = { ...makeSession(), panels: ['r1'] };
    expect(closePanel(s, 'r2')).toBe(s);
  });
});

describe('queue-aware agent-tail reducers', () => {
  const userT = (text) => ({ role: 'user', ts: '', blocks: [{ type: 'text', text }] });
  const agentT = (text = '') => ({ role: 'agent', ts: '', blocks: [{ type: 'text', text }] });

  it('lastAgentIndex skips trailing user turns', () => {
    const s = { ...makeSession(), liveTurns: [userT('a'), agentT('hi'), userT('b')] };
    expect(lastAgentIndex(s)).toBe(1);
  });

  it('lastAgentIndex returns -1 when no agent turn exists', () => {
    const s = { ...makeSession(), liveTurns: [userT('a'), userT('b')] };
    expect(lastAgentIndex(s)).toBe(-1);
  });

  it('mapAgentTail routes deltas to the in-flight agent turn even with a queued user turn at the tail', () => {
    // Reproduces the queue race: [user, agent-filling, user-queued].
    // A delta arriving before the queue-drain Done belongs to the
    // middle agent turn, NOT to the trailing user turn.
    const s = {
      ...makeSession(),
      liveTurns: [userT('q1'), agentT('partial'), userT('q2')],
    };
    const next = mapAgentTail(s, t => ({
      ...t, blocks: [{ type: 'text', text: t.blocks[0].text + ' more' }],
    }));
    expect(next.liveTurns).toHaveLength(3);
    expect(next.liveTurns[1].blocks[0].text).toBe('partial more');
    expect(next.liveTurns[2]).toBe(s.liveTurns[2]);
  });

  it('mapAgentTail mints a fresh agent turn after Done (nextAgentNew)', () => {
    // Done sets nextAgentNew=true.  The next delta must NOT graft
    // onto the just-finished turn — it belongs to the queue-drain
    // reply that the server is about to start.
    const s = {
      ...makeSession(),
      nextAgentNew: true,
      running: false,
      liveTurns: [userT('q1'), agentT('done'), userT('q2')],
    };
    const next = mapAgentTail(s, t => ({
      ...t, blocks: [{ type: 'text', text: 'fresh delta' }],
    }));
    expect(next.liveTurns).toHaveLength(4);
    expect(next.liveTurns[3].role).toBe('agent');
    expect(next.liveTurns[3].blocks[0].text).toBe('fresh delta');
    // The previous agent turn must be untouched.
    expect(next.liveTurns[1]).toBe(s.liveTurns[1]);
    expect(next.nextAgentNew).toBe(false);
    // Typing indicator returns for the drained reply.
    expect(next.running).toBe(true);
  });

  it('mapAgentTail creates an agent turn when none exists yet', () => {
    const s = { ...makeSession(), liveTurns: [userT('only-user')] };
    const next = mapAgentTail(s, t => ({
      ...t, blocks: [{ type: 'text', text: 'first delta' }],
    }));
    expect(next.liveTurns).toHaveLength(2);
    expect(next.liveTurns[1].role).toBe('agent');
    expect(next.running).toBe(true);
  });

  it('appendAgentBlock pushes onto the in-flight agent turn, not a trailing user turn', () => {
    const s = {
      ...makeSession(),
      liveTurns: [userT('q1'), agentT('hi'), userT('q2')],
    };
    const next = appendAgentBlock(s, { type: 'tool', ref: 'r1' });
    expect(next.liveTurns[1].blocks).toHaveLength(2);
    expect(next.liveTurns[1].blocks[1]).toEqual({ type: 'tool', ref: 'r1' });
    expect(next.liveTurns[2].blocks).toHaveLength(1);
  });
});
