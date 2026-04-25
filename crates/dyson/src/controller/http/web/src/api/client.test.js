import { describe, it, expect, vi } from 'vitest';
import { DysonClient } from './client.js';

// Mock fetch helper: builds a fetch whose resolved Response carries
// `json()` and `text()` implementations so the client's await chain
// behaves like the real thing without a real network.
function mockFetch(makeResponse) {
  return vi.fn(async (...args) => {
    const r = makeResponse(...args);
    return {
      ok: true,
      status: 200,
      headers: new Headers(r.headers || {}),
      json: async () => r.body,
      text: async () => (typeof r.body === 'string' ? r.body : JSON.stringify(r.body)),
      blob: async () => r.blob || new Blob([typeof r.body === 'string' ? r.body : JSON.stringify(r.body)]),
      ...r.override,
    };
  });
}

function args(fetchSpy, call = 0) {
  return fetchSpy.mock.calls[call];
}

describe('DysonClient — constructor', () => {
  it('throws when no fetch is available', () => {
    expect(() => new DysonClient({ fetch: null })).toThrow(/fetch/);
  });
});

describe('DysonClient — GET endpoints', () => {
  it('listConversations → GET /api/conversations', async () => {
    const fetch = mockFetch(() => ({ body: [{ id: 'a', title: 't' }] }));
    const client = new DysonClient({ fetch });
    const out = await client.listConversations();
    expect(args(fetch)[0]).toBe('/api/conversations');
    expect(new Headers(args(fetch)[1].headers).get('accept')).toBe('application/json');
    expect(out).toEqual([{ id: 'a', title: 't' }]);
  });

  it('listProviders → GET /api/providers', async () => {
    const fetch = mockFetch(() => ({ body: [] }));
    const client = new DysonClient({ fetch });
    await client.listProviders();
    expect(args(fetch)[0]).toBe('/api/providers');
  });

  it('getMind → GET /api/mind', async () => {
    const fetch = mockFetch(() => ({ body: { files: [] } }));
    const client = new DysonClient({ fetch });
    await client.getMind();
    expect(args(fetch)[0]).toBe('/api/mind');
  });

  it('getActivity → GET /api/activity (no chat filter)', async () => {
    const fetch = mockFetch(() => ({ body: { lanes: [] } }));
    const client = new DysonClient({ fetch });
    await client.getActivity();
    expect(args(fetch)[0]).toBe('/api/activity');
  });

  it('getActivity → GET /api/activity?chat=<encoded> with a chat id', async () => {
    const fetch = mockFetch(() => ({ body: { lanes: [] } }));
    const client = new DysonClient({ fetch });
    await client.getActivity('chat/123');
    expect(args(fetch)[0]).toBe('/api/activity?chat=chat%2F123');
  });

  it('load → GET /api/conversations/:id (encoded)', async () => {
    const fetch = mockFetch(() => ({ body: { messages: [] } }));
    const client = new DysonClient({ fetch });
    await client.load('c 1');
    expect(args(fetch)[0]).toBe('/api/conversations/c%201');
  });

  it('mindFile → GET /api/mind/file?path=<encoded>', async () => {
    const fetch = mockFetch(() => ({ body: { content: '' } }));
    const client = new DysonClient({ fetch });
    await client.mindFile('a/b.md');
    expect(args(fetch)[0]).toBe('/api/mind/file?path=a%2Fb.md');
  });

  it('loadFeedback → GET /api/conversations/:id/feedback (returns [] on non-ok)', async () => {
    const fetch = vi.fn(async () => ({ ok: false, status: 404, json: async () => ([]) }));
    const client = new DysonClient({ fetch });
    const out = await client.loadFeedback('c1');
    expect(out).toEqual([]);
    expect(args(fetch)[0]).toBe('/api/conversations/c1/feedback');
  });

  it('listArtefacts → GET /api/conversations/:id/artefacts (returns [] on non-ok)', async () => {
    const fetch = vi.fn(async () => ({ ok: false, status: 404, json: async () => ([]) }));
    const client = new DysonClient({ fetch });
    const out = await client.listArtefacts('c1');
    expect(out).toEqual([]);
    expect(args(fetch)[0]).toBe('/api/conversations/c1/artefacts');
  });

  it('loadArtefact returns body + chat id from X-Dyson-Chat-Id', async () => {
    const fetch = vi.fn(async () => ({
      ok: true,
      status: 200,
      headers: new Headers({ 'X-Dyson-Chat-Id': 'c1' }),
      text: async () => '# report',
    }));
    const client = new DysonClient({ fetch });
    const out = await client.loadArtefact('a1');
    expect(out).toEqual({ body: '# report', chatId: 'c1' });
    expect(args(fetch)[0]).toBe('/api/artefacts/a1');
  });

  it('exportConversation returns the response blob', async () => {
    const blob = new Blob(['{}'], { type: 'application/json' });
    const fetch = vi.fn(async () => ({ ok: true, status: 200, blob: async () => blob }));
    const client = new DysonClient({ fetch });
    const out = await client.exportConversation('c1');
    expect(out).toBe(blob);
    expect(args(fetch)[0]).toBe('/api/conversations/c1/export');
  });
});

describe('DysonClient — POST endpoints', () => {
  it('createChat → POST /api/conversations with title', async () => {
    const fetch = mockFetch(() => ({ body: { id: 'new' } }));
    const client = new DysonClient({ fetch });
    await client.createChat('My chat');
    const [url, init] = args(fetch);
    expect(url).toBe('/api/conversations');
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body)).toEqual({ title: 'My chat' });
  });

  it('createChat passes rotate_previous when supplied', async () => {
    const fetch = mockFetch(() => ({ body: { id: 'new' } }));
    const client = new DysonClient({ fetch });
    await client.createChat('x', 'old-id');
    expect(JSON.parse(args(fetch)[1].body)).toEqual({ title: 'x', rotate_previous: 'old-id' });
  });

  it('deleteChat → DELETE /api/conversations/:id', async () => {
    const fetch = mockFetch(() => ({ body: { deleted: true } }));
    const client = new DysonClient({ fetch });
    await client.deleteChat('c1');
    expect(args(fetch)[0]).toBe('/api/conversations/c1');
    expect(args(fetch)[1].method).toBe('DELETE');
  });

  it('postMindFile → POST /api/mind/file with path + content', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    const client = new DysonClient({ fetch });
    await client.postMindFile('a.md', 'body');
    const [url, init] = args(fetch);
    expect(url).toBe('/api/mind/file');
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body)).toEqual({ path: 'a.md', content: 'body' });
  });

  it('postMindFile throws on non-ok', async () => {
    const fetch = vi.fn(async () => ({ ok: false, status: 500 }));
    const client = new DysonClient({ fetch });
    await expect(client.postMindFile('a', 'b')).rejects.toThrow(/save failed/);
  });

  it('feedback → POST /api/conversations/:id/feedback with turn_index + emoji', async () => {
    const fetch = mockFetch(() => ({ body: { ok: true } }));
    const client = new DysonClient({ fetch });
    await client.feedback('c1', 3, '👍');
    const [url, init] = args(fetch);
    expect(url).toBe('/api/conversations/c1/feedback');
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body)).toEqual({ turn_index: 3, emoji: '👍' });
  });

  it('postModel → POST /api/model with provider + model', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    const client = new DysonClient({ fetch });
    await client.postModel('p1', 'm1');
    const [url, init] = args(fetch);
    expect(url).toBe('/api/model');
    expect(init.method).toBe('POST');
    expect(JSON.parse(init.body)).toEqual({ provider: 'p1', model: 'm1' });
  });

  it('cancel → POST /api/conversations/:id/cancel', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    const client = new DysonClient({ fetch });
    await client.cancel('c1');
    expect(args(fetch)[0]).toBe('/api/conversations/c1/cancel');
    expect(args(fetch)[1].method).toBe('POST');
  });
});

describe('DysonClient._authedFetch — bearer token plumbing', () => {
  it('attaches Authorization on every method that hits /api/*', async () => {
    // Constructor takes a getToken zero-arg fn.  Each request type
    // must pull a fresh value (so a silent refresh propagates) and
    // stamp it as `Authorization: Bearer <token>` exactly once.
    const fetch = mockFetch(() => ({ body: {} }));
    const getToken = vi.fn(() => 'TOK');
    const client = new DysonClient({ fetch, getToken, EventSource: class {
      close() {}
      set onmessage(_v) {}
    } });

    await client.listConversations();
    await client.listProviders();
    await client.getMind();
    await client.load('c1');
    await client.deleteChat('c1');
    await client.feedback('c1', 0, '👍');
    await client.postModel('p', 'm');
    await client.cancel('c1');
    await client.loadFeedback('c1');
    await client.listArtefacts('c1');

    // Every recorded fetch call carries Authorization: Bearer TOK.
    for (const [, init] of fetch.mock.calls) {
      const auth = (init?.headers && (init.headers.get
        ? init.headers.get('authorization')
        : init.headers.authorization || init.headers.Authorization)) || null;
      expect(auth, `call to ${fetch.mock.calls[0][0]} missing auth`).toBe('Bearer TOK');
    }
    // getToken was invoked at least once per request (no caching of a
    // stale value across the silent-refresh window).
    expect(getToken.mock.calls.length).toBeGreaterThanOrEqual(fetch.mock.calls.length);
  });

  it('does not add Authorization when getToken returns null', async () => {
    const fetch = mockFetch(() => ({ body: [] }));
    const client = new DysonClient({ fetch, getToken: () => null });
    await client.listConversations();
    const init = fetch.mock.calls[0][1] || {};
    const headers = new Headers(init.headers || {});
    expect(headers.get('authorization')).toBeNull();
  });

  it('stamps X-Dyson-CSRF on every request even without a bearer', async () => {
    // Server rejects state-changing /api/* requests that don't carry
    // the custom CSRF marker — browsers can't add it cross-origin
    // without a CORS preflight, which the controller refuses.  The
    // wrapper must add it on every request (mutating or not) so a
    // future GET-turned-POST doesn't silently 400.
    const fetch = mockFetch(() => ({ body: {} }));
    const client = new DysonClient({ fetch, getToken: () => null });
    await client.listConversations();
    await client.deleteChat('c1');
    await client.postModel('p', 'm');
    for (const [, init] of fetch.mock.calls) {
      const headers = new Headers(init.headers || {});
      expect(headers.get('x-dyson-csrf')).toBe('1');
    }
  });

  it('preserves an existing Authorization header from the caller', async () => {
    // _authedFetch only sets the header when none is present.  An
    // explicit Authorization passed through the init object must
    // survive — protects callers that override per-request (e.g. a
    // future MCP-bridge adapter).
    const fetch = mockFetch(() => ({ body: [] }));
    const client = new DysonClient({ fetch, getToken: () => 'OVERRIDE_ME' });
    await client._authedFetch('/api/anything', {
      headers: { Authorization: 'Bearer caller-supplied' },
    });
    const headers = new Headers(fetch.mock.calls[0][1].headers);
    expect(headers.get('authorization')).toBe('Bearer caller-supplied');
  });

  it('SSE flow exchanges the bearer for a ticket before opening EventSource', async () => {
    // The raw bearer must never reach the URL — leaks into history /
    // proxy logs / Referer.  Send first POSTs /api/auth/sse-ticket
    // (header-bearer) and uses the returned ticket as access_token.
    const fetch = vi.fn(async (url) => {
      if (url === '/api/auth/sse-ticket') {
        return {
          ok: true,
          status: 200,
          headers: new Headers(),
          json: async () => ({ ticket: 'one-shot-abc' }),
        };
      }
      return { ok: true, status: 200 };
    });
    class FakeES {
      constructor(url) { FakeES.lastUrl = url; this.onmessage = null; }
      close() {}
    }
    const client = new DysonClient({
      fetch,
      EventSource: FakeES,
      getToken: () => 'TOK-XYZ',
    });
    client.send('c1', 'hi', {});
    // Wait for ticket fetch + EventSource open.
    await new Promise(r => setTimeout(r, 0));
    await new Promise(r => setTimeout(r, 0));
    // The mint call goes out first with the bearer in a header.
    const mint = fetch.mock.calls.find(c => c[0] === '/api/auth/sse-ticket');
    expect(mint, 'must POST /api/auth/sse-ticket').toBeTruthy();
    const mintHeaders = new Headers(mint[1].headers || {});
    expect(mintHeaders.get('authorization')).toBe('Bearer TOK-XYZ');
    // EventSource opens with the *ticket*, not the raw bearer.
    expect(FakeES.lastUrl).toBe('/api/conversations/c1/events?access_token=one-shot-abc');
    expect(FakeES.lastUrl).not.toContain('TOK-XYZ');
  });

  it('no-auth deployment opens EventSource without a ticket', async () => {
    // dangerous_no_auth: getToken returns null, no ticket exchange,
    // EventSource hits the bare URL.
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    class FakeES {
      constructor(url) { FakeES.lastUrl = url; this.onmessage = null; }
      close() {}
    }
    const client = new DysonClient({
      fetch,
      EventSource: FakeES,
      getToken: () => null,
    });
    client.send('c1', 'hi', {});
    await new Promise(r => setTimeout(r, 0));
    expect(FakeES.lastUrl).toBe('/api/conversations/c1/events');
    // No mint call when there's nothing to exchange.
    expect(fetch.mock.calls.find(c => c[0] === '/api/auth/sse-ticket')).toBeUndefined();
  });
});

describe('DysonClient — send (SSE + POST turn)', () => {
  it('opens an EventSource (post-ticket), POSTs the turn body, returns a closeable handle', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    class FakeES {
      constructor(url) { FakeES.lastUrl = url; this.onmessage = null; }
      close() { this.closed = true; }
    }
    const client = new DysonClient({ fetch, EventSource: FakeES });
    const handle = client.send('c1', 'hello', {});
    // Caller gets a `{ close }`-shaped object (ticket exchange may
    // open the ES asynchronously; the handle wraps that).
    expect(typeof handle.close).toBe('function');
    // Yield twice — once for the ticket-exchange `then`, once for the
    // open-stream resolution.
    await new Promise(r => setTimeout(r, 0));
    await new Promise(r => setTimeout(r, 0));
    // No-auth deployment skips the ticket call and opens the bare URL.
    expect(FakeES.lastUrl).toBe('/api/conversations/c1/events');
    const turnCall = fetch.mock.calls.find(c => c[0] === '/api/conversations/c1/turn');
    expect(turnCall).toBeTruthy();
    expect(JSON.parse(turnCall[1].body)).toEqual({ prompt: 'hello', attachments: [] });
  });

  it('incoming events dispatch through stream.js callbacks', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    let instance;
    class FakeES {
      constructor() { this.onmessage = null; instance = this; }
      close() {}
    }
    const client = new DysonClient({ fetch, EventSource: FakeES });
    const onText = vi.fn();
    client.send('c1', 'hi', { onText });
    // Wait for the EventSource to open after the (no-op) ticket path.
    await new Promise(r => setTimeout(r, 0));
    await new Promise(r => setTimeout(r, 0));
    instance.onmessage({ data: JSON.stringify({ type: 'text', delta: 'd' }) });
    expect(onText).toHaveBeenCalledWith('d');
  });

  it('turn POST non-ok closes the stream and fires onError', async () => {
    const fetch = vi.fn(async () => ({ ok: false, status: 400 }));
    let instance;
    class FakeES {
      constructor() { instance = this; this.closed = false; }
      close() { this.closed = true; }
    }
    const client = new DysonClient({ fetch, EventSource: FakeES });
    const onError = vi.fn();
    client.send('c1', 'hi', { onError });
    await new Promise(r => setTimeout(r, 0));
    expect(instance.closed).toBe(true);
    expect(onError).toHaveBeenCalledWith(expect.stringContaining('turn rejected: 400'));
  });
});
