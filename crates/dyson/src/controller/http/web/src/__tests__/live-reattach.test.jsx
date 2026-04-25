// Regression coverage for the "mid-stream reload blanks the chat" fix.
//
// Single piece: attachLiveStream() opens an SSE stream against a busy
// chat WITHOUT POSTing /turn (which would 409 against the in-flight
// turn we're trying to watch).  It also seeds an empty agent
// placeholder so streamCallbacks.onText has somewhere to write its
// deltas.

import { describe, it, expect, beforeEach, vi } from 'vitest';
import {
  ensureSession,
  updateSession,
  getSession,
  __resetSessionsForTests,
} from '../store/sessions.js';
import { __resetAppStoreForTests } from '../store/app.js';
import { attachLiveStream } from '../components/app.jsx';

beforeEach(() => {
  __resetSessionsForTests();
  __resetAppStoreForTests();
});

describe('attachLiveStream', () => {
  function mockClient() {
    return {
      attach: vi.fn(() => ({ close: vi.fn() })),
      // Defensive: send() must NOT be called.  attach() is the
      // turn-free observation path; calling send() would POST /turn
      // and 409 against the in-flight turn we're watching.
      send: vi.fn(() => { throw new Error('attachLiveStream must not call client.send'); }),
    };
  }

  it('opens the SSE stream via client.attach (not send)', () => {
    ensureSession('c1');
    const client = mockClient();
    attachLiveStream('c1', client);
    expect(client.attach).toHaveBeenCalledTimes(1);
    expect(client.attach.mock.calls[0][0]).toBe('c1');
    // Second arg is the streamCallbacks bag — at minimum it must
    // carry onText (the hot path).
    expect(typeof client.attach.mock.calls[0][1].onText).toBe('function');
    expect(client.send).not.toHaveBeenCalled();
  });

  it('appends an empty agent placeholder so onText has somewhere to write', () => {
    // Disk hydrate gives us the user message that started the in-
    // flight turn (committed by the persist hook).  attachLiveStream
    // adds the matching agent placeholder so the SSE text deltas
    // (which target the LAST turn) land correctly.
    updateSession('c1', s => ({ ...s, liveTurns: [
      { role: 'user', ts: '', blocks: [{ type: 'text', text: 'hi' }] },
    ]}));
    attachLiveStream('c1', mockClient());
    const turns = getSession('c1').liveTurns;
    expect(turns).toHaveLength(2);
    expect(turns[1].role).toBe('agent');
    expect(turns[1].blocks).toEqual([{ type: 'text', text: '' }]);
  });

  it('flips running to true so the typing indicator paints', () => {
    ensureSession('c1');
    attachLiveStream('c1', mockClient());
    expect(getSession('c1').running).toBe(true);
  });

  it('does not double-add a placeholder when the cache already has an agent turn waiting', () => {
    // E.g. the cache survived the reload with the placeholder in it.
    // We just need to attach the stream — appending another agent
    // turn would split the deltas across two empty placeholders.
    updateSession('c1', s => ({ ...s, liveTurns: [
      { role: 'user', ts: '', blocks: [{ type: 'text', text: 'hi' }] },
      { role: 'agent', ts: '', blocks: [{ type: 'text', text: '' }] },
    ]}));
    attachLiveStream('c1', mockClient());
    expect(getSession('c1').liveTurns).toHaveLength(2);
  });
});
