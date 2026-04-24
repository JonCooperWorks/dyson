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
    expect(args(fetch)[1].headers.Accept).toBe('application/json');
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

describe('DysonClient — send (SSE + POST turn)', () => {
  it('opens an EventSource, POSTs the turn body, returns the ES', async () => {
    const fetch = vi.fn(async () => ({ ok: true, status: 200 }));
    class FakeES {
      constructor(url) { FakeES.lastUrl = url; this.onmessage = null; }
      close() { this.closed = true; }
    }
    const client = new DysonClient({ fetch, EventSource: FakeES });
    const es = client.send('c1', 'hello', {});
    expect(es).toBeInstanceOf(FakeES);
    expect(FakeES.lastUrl).toBe('/api/conversations/c1/events');
    // Let the buildBody microtask resolve before checking fetch.
    await new Promise(r => setTimeout(r, 0));
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
