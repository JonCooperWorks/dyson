/* Dyson — HTTP + SSE client for the HttpController.
 *
 * Every method maps to one endpoint in controller/http/mod.rs.  The
 * constructor takes a `fetch` implementation and an optional EventSource
 * factory so tests can inject mocks without touching globals — the old
 * shape jammed everything on `window.DysonLive` and made the transport
 * inseparable from the browser. */

import { parseStreamEvent, dispatchStreamEvent } from './stream.js';

// File → base64 (no data-URL prefix).  Used when the composer attaches
// uploads to a turn; matches the server's attachment schema.
function fileToBase64(file) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const s = r.result || '';
      const i = typeof s === 'string' ? s.indexOf(',') : -1;
      resolve(i >= 0 ? s.slice(i + 1) : '');
    };
    r.onerror = () => reject(r.error || new Error('read failed'));
    r.readAsDataURL(file);
  });
}

export class DysonClient {
  constructor({ fetch: fetchImpl, EventSource: EventSourceImpl, getToken } = {}) {
    // Respect explicit null — tests pass `{ fetch: null }` to assert the
    // guard fires.  Only fall back to the global when the field is
    // missing entirely.
    const globalFetch = typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null;
    const globalES = typeof globalThis.EventSource === 'function' ? globalThis.EventSource : null;
    this._fetch = fetchImpl === undefined ? globalFetch : fetchImpl;
    this._EventSource = EventSourceImpl === undefined ? globalES : EventSourceImpl;
    // `getToken` is a zero-arg fn returning the current OIDC access
    // token (or null in dangerous_no_auth mode).  Called fresh per
    // request so silent refreshes propagate without rebuilding the
    // client.
    this._getToken = typeof getToken === 'function' ? getToken : () => null;
    if (!this._fetch) throw new Error('DysonClient: no fetch implementation available');
  }

  // Inject Authorization (when available) plus the X-Dyson-CSRF marker
  // on every request.  The CSRF header is the controller's anti-CSRF
  // gate: the server rejects any state-changing /api/* call that's
  // missing it, and browsers won't let a cross-origin page set a custom
  // header without a CORS preflight (which the controller refuses).
  // Stamping it on every request — not just mutating ones — keeps the
  // wrapper one-line and means a future GET that becomes mutating
  // doesn't silently 400.
  _authedFetch(url, init) {
    const headers = new Headers((init && init.headers) || {});
    if (!headers.has('x-dyson-csrf')) headers.set('x-dyson-csrf', '1');
    const token = this._getToken();
    if (token && !headers.has('authorization')) {
      headers.set('authorization', `Bearer ${token}`);
    }
    return this._fetch(url, { ...(init || {}), headers });
  }

  async _json(url, init) {
    const r = await this._authedFetch(url, init);
    if (!r.ok) throw new Error(`${(init && init.method) || 'GET'} ${url}: ${r.status}`);
    return r.json();
  }

  listConversations() {
    return this._json('/api/conversations', { headers: { Accept: 'application/json' } });
  }

  listProviders() { return this._json('/api/providers'); }
  getMind()       { return this._json('/api/mind'); }

  getActivity(chatId) {
    const qs = chatId ? `?chat=${encodeURIComponent(chatId)}` : '';
    return this._json(`/api/activity${qs}`);
  }

  // Rotate the prior chat's transcript (archive it) when minting a new
  // one.  Used by /clear and the "+ New Conversation" button when the
  // user wants the walk-away to be preserved.
  createChat(title, rotatePrevious) {
    const body = { title: title || 'New conversation' };
    if (rotatePrevious) body.rotate_previous = rotatePrevious;
    return this._json('/api/conversations', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }

  load(id) {
    return this._json(`/api/conversations/${encodeURIComponent(id)}`);
  }

  // Server rotates non-empty chats (transcript survives as a dated
  // archive) and hard-deletes empty ones.  Returns { deleted, preserved }.
  deleteChat(id) {
    return this._json(`/api/conversations/${encodeURIComponent(id)}`, { method: 'DELETE' });
  }

  mindFile(path) {
    return this._json(`/api/mind/file?path=${encodeURIComponent(path)}`);
  }

  async postMindFile(path, content) {
    const r = await this._authedFetch('/api/mind/file', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, content }),
    });
    if (!r.ok) throw new Error(`save failed: ${r.status}`);
    return r;
  }

  async feedback(chatId, turnIndex, emoji) {
    const r = await this._authedFetch(`/api/conversations/${encodeURIComponent(chatId)}/feedback`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ turn_index: turnIndex, emoji: emoji || '' }),
    });
    if (!r.ok) throw new Error(`feedback failed: ${r.status}`);
    return r.json();
  }

  async loadFeedback(chatId) {
    const r = await this._authedFetch(`/api/conversations/${encodeURIComponent(chatId)}/feedback`);
    if (!r.ok) return [];
    return r.json();
  }

  async listArtefacts(chatId) {
    const r = await this._authedFetch(`/api/conversations/${encodeURIComponent(chatId)}/artefacts`);
    if (!r.ok) return [];
    return r.json();
  }

  // Artefact bodies are text: markdown for reports, the served file URL
  // for images (stored verbatim).  The `X-Dyson-Chat-Id` response header
  // lets a cold deep-link (/#/artefacts/<id> pasted into a fresh tab)
  // restore the sidebar context without a second round-trip.
  async loadArtefact(id) {
    const r = await this._authedFetch(`/api/artefacts/${encodeURIComponent(id)}`);
    if (!r.ok) throw new Error(`artefact load failed: ${r.status}`);
    const body = await r.text();
    const chatId = r.headers.get('X-Dyson-Chat-Id') || null;
    return { body, chatId };
  }

  // ShareGPT export — controller returns a JSON blob.  Returning the
  // Blob instead of triggering the save lets the caller choose between
  // an anchor-click download (browser) or a file-write (future desktop
  // shell).
  async exportConversation(chatId) {
    const r = await this._authedFetch(`/api/conversations/${encodeURIComponent(chatId)}/export`);
    if (!r.ok) {
      const txt = await r.text().catch(() => '');
      throw new Error(`export failed: ${r.status}${txt ? ` — ${txt}` : ''}`);
    }
    return r.blob();
  }

  async postModel(provider, model) {
    const r = await this._authedFetch('/api/model', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ provider, model }),
    });
    if (!r.ok) throw new Error(`set model failed: ${r.status}`);
    return r;
  }

  // Open the SSE stream, POST the turn, and dispatch each incoming
  // event through the callback bag.  Returns a wrapper that mimics
  // EventSource (`close()`) so the caller can tear the stream down on
  // cancel even when the EventSource itself is opened asynchronously
  // after the ticket exchange.
  //
  // `files` is an optional list of File objects (from <input type=file>
  // or drag-drop).  Each is read into base64 and sent as an attachment —
  // controller dispatches through Agent::run_with_attachments (same path
  // Telegram uses for photos / voice notes / docs).
  //
  // Auth shape: EventSource can't send headers, so for any deployment
  // with auth enabled the SPA exchanges its bearer for a one-shot SSE
  // ticket (POST /api/auth/sse-ticket).  The controller hands the
  // ticket back as an HttpOnly cookie scoped to /api/conversations,
  // and the browser attaches it automatically when EventSource opens
  // — no token in URL history, proxy logs, or the referrer chain.
  // Open the SSE stream WITHOUT POSTing a turn.  Internal helper used
  // by both send() (which adds a /turn POST) and attach() (which
  // doesn't — it's purely observational, for re-attaching to an
  // already-running turn after a page reload).  Returns the same
  // `{ close, _es }` shape as send().
  _openEvents(id, callbacks) {
    if (!this._EventSource) throw new Error('DysonClient: no EventSource implementation');
    const cb = callbacks || {};
    let es = null;
    let closed = false;

    const openStream = (eventsUrl) => {
      if (closed) return;
      es = new this._EventSource(eventsUrl, { withCredentials: true });
      es.onmessage = (ev) => {
        const msg = parseStreamEvent(ev.data);
        if (!msg) return;
        if (msg.type === 'done') es.close();
        dispatchStreamEvent(msg, cb);
      };
    };

    const eventsBase = `/api/conversations/${encodeURIComponent(id)}/events`;
    const ticketed = async () => {
      const tok = this._getToken();
      if (!tok) return eventsBase;
      const r = await this._authedFetch('/api/auth/sse-ticket', { method: 'POST' });
      if (!r.ok) throw new Error(`ticket mint failed: ${r.status}`);
      return eventsBase;
    };

    ticketed()
      .then(openStream)
      .catch(e => { closed = true; cb.onError && cb.onError(`sse open failed: ${e.message}`); });

    return {
      close() { closed = true; if (es) es.close(); },
      get _es() { return es; },
      // Internal hooks so send() can tear down the stream from the
      // POST-failure path.
      _markClosed() { closed = true; if (es) es.close(); },
    };
  }

  send(id, prompt, callbacks, files) {
    const cb = callbacks || {};
    const handle = this._openEvents(id, cb);

    const buildBody = async () => {
      const attachments = [];
      for (const f of (files || [])) {
        attachments.push({
          name: f.name,
          mime_type: f.type || 'application/octet-stream',
          data_base64: await fileToBase64(f),
        });
      }
      return JSON.stringify({ prompt, attachments });
    };

    buildBody()
      .then(body => this._authedFetch(`/api/conversations/${encodeURIComponent(id)}/turn`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
      }))
      .then(r => {
        if (!r.ok) {
          handle._markClosed();
          cb.onError && cb.onError(`turn rejected: ${r.status}`);
        }
      })
      .catch(e => {
        handle._markClosed();
        cb.onError && cb.onError(`turn failed: ${e.message}`);
      });

    return handle;
  }

  // Attach to an in-flight chat's SSE stream without POSTing a turn.
  // Used after a mid-stream page reload: the original POST already
  // hit the server (busy = true) and a second POST would 409.  The
  // server's replay ring will yield any events buffered since the
  // turn started, then the live broadcast keeps appending.
  attach(id, callbacks) {
    return this._openEvents(id, callbacks);
  }

  async cancel(id) {
    await this._authedFetch(`/api/conversations/${encodeURIComponent(id)}/cancel`, { method: 'POST' });
  }
}
