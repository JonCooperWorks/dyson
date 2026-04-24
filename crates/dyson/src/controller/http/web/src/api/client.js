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
  constructor({ fetch: fetchImpl, EventSource: EventSourceImpl } = {}) {
    // Respect explicit null — tests pass `{ fetch: null }` to assert the
    // guard fires.  Only fall back to the global when the field is
    // missing entirely.
    const globalFetch = typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null;
    const globalES = typeof globalThis.EventSource === 'function' ? globalThis.EventSource : null;
    this._fetch = fetchImpl === undefined ? globalFetch : fetchImpl;
    this._EventSource = EventSourceImpl === undefined ? globalES : EventSourceImpl;
    if (!this._fetch) throw new Error('DysonClient: no fetch implementation available');
  }

  async _json(url, init) {
    const r = await this._fetch(url, init);
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
    const r = await this._fetch('/api/mind/file', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, content }),
    });
    if (!r.ok) throw new Error(`save failed: ${r.status}`);
    return r;
  }

  async feedback(chatId, turnIndex, emoji) {
    const r = await this._fetch(`/api/conversations/${encodeURIComponent(chatId)}/feedback`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ turn_index: turnIndex, emoji: emoji || '' }),
    });
    if (!r.ok) throw new Error(`feedback failed: ${r.status}`);
    return r.json();
  }

  async loadFeedback(chatId) {
    const r = await this._fetch(`/api/conversations/${encodeURIComponent(chatId)}/feedback`);
    if (!r.ok) return [];
    return r.json();
  }

  async listArtefacts(chatId) {
    const r = await this._fetch(`/api/conversations/${encodeURIComponent(chatId)}/artefacts`);
    if (!r.ok) return [];
    return r.json();
  }

  // Artefact bodies are text: markdown for reports, the served file URL
  // for images (stored verbatim).  The `X-Dyson-Chat-Id` response header
  // lets a cold deep-link (/#/artefacts/<id> pasted into a fresh tab)
  // restore the sidebar context without a second round-trip.
  async loadArtefact(id) {
    const r = await this._fetch(`/api/artefacts/${encodeURIComponent(id)}`);
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
    const r = await this._fetch(`/api/conversations/${encodeURIComponent(chatId)}/export`);
    if (!r.ok) {
      const txt = await r.text().catch(() => '');
      throw new Error(`export failed: ${r.status}${txt ? ` — ${txt}` : ''}`);
    }
    return r.blob();
  }

  async postModel(provider, model) {
    const r = await this._fetch('/api/model', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ provider, model }),
    });
    if (!r.ok) throw new Error(`set model failed: ${r.status}`);
    return r;
  }

  // Open the SSE stream, POST the turn, and dispatch each incoming
  // event through the callback bag.  Returns the EventSource so the
  // caller can close() on cancel.
  //
  // `files` is an optional list of File objects (from <input type=file>
  // or drag-drop).  Each is read into base64 and sent as an attachment —
  // controller dispatches through Agent::run_with_attachments (same path
  // Telegram uses for photos / voice notes / docs).
  send(id, prompt, callbacks, files) {
    if (!this._EventSource) throw new Error('DysonClient.send: no EventSource implementation');
    const cb = callbacks || {};
    const es = new this._EventSource(`/api/conversations/${encodeURIComponent(id)}/events`);
    es.onmessage = (ev) => {
      const msg = parseStreamEvent(ev.data);
      if (!msg) return;
      if (msg.type === 'done') es.close();
      dispatchStreamEvent(msg, cb);
    };

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
      .then(body => this._fetch(`/api/conversations/${encodeURIComponent(id)}/turn`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
      }))
      .then(r => {
        if (!r.ok) { es.close(); cb.onError && cb.onError(`turn rejected: ${r.status}`); }
      })
      .catch(e => {
        es.close(); cb.onError && cb.onError(`turn failed: ${e.message}`);
      });
    return es;
  }

  async cancel(id) {
    await this._fetch(`/api/conversations/${encodeURIComponent(id)}/cancel`, { method: 'POST' });
  }
}
