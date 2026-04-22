// Bridge between the prototype shell and the live HttpController API.
//
// Probes /api/conversations on load.  When the API answers we wipe every
// seed section in DYSON_DATA and populate from the controller — no fake
// data, no fallback fakes if a fetch fails (the section just stays
// empty).  Wire format: crates/dyson/src/controller/http/mod.rs.
(function () {
  const D = window.DYSON_DATA;
  if (!D) return;

  fetch('/api/conversations', { headers: { Accept: 'application/json' } })
    .then(r => r.ok ? r.json() : Promise.reject(r.status))
    .then(activate)
    .catch(() => console.info('[dyson] live API not reachable — staying in seed-data mode'));

  function activate(list) {
    window.DYSON_LIVE = true;

    D.conversations = { http: [] };
    D.providers = [];
    D.activity = [];
    D.mind = { backend: '', files: [], open: { path: '', content: '' } };
    D.tools = {};

    D.conversations.http = list.map(c => ({
      id: c.id,
      title: c.title,
      live: !!c.live,
    }));

    fetch('/api/providers').then(r => r.json()).then(provs => {
      D.providers = (provs || []).map(p => ({
        id: p.id,
        name: p.name,
        models: p.models || [],
        activeModel: p.active_model || '',
        active: !!p.active,
      }));
      // Active model is the first provider's active_model — controller
      // sorts the active provider first.
      D.activeModel = (D.providers[0] && D.providers[0].activeModel) || '';
      window.dispatchEvent(new CustomEvent('dyson:live-update'));
    });

    fetch('/api/mind').then(r => r.json()).then(mind => {
      D.mind.backend = mind.backend || '';
      D.mind.files = (mind.files || []).map(f => ({
        path: f.path,
        size: typeof f.size === 'number' ? `${f.size} B` : '',
      }));
      window.dispatchEvent(new CustomEvent('dyson:live-update'));
    });

    fetch('/api/activity').then(r => r.json()).then(act => {
      D.activity = act.lanes || [];
      window.dispatchEvent(new CustomEvent('dyson:live-update'));
    });

    window.DysonLive = {
      createChat: async (title) => {
        const r = await fetch('/api/conversations', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ title: title || 'New conversation' }),
        });
        if (!r.ok) throw new Error('create failed: ' + r.status);
        return r.json();
      },

      load: async (id) => {
        const r = await fetch('/api/conversations/' + encodeURIComponent(id));
        if (!r.ok) throw new Error('load failed: ' + r.status);
        return r.json();
      },

      mindFile: async (path) => {
        const r = await fetch('/api/mind/file?path=' + encodeURIComponent(path));
        if (!r.ok) throw new Error('mind file failed: ' + r.status);
        return r.json();
      },

      // Per-turn rating (Telegram-equivalent).  emoji='' removes.
      // Returns the saved entry or { ok:true, removed:true }.
      feedback: async (chatId, turnIndex, emoji) => {
        const r = await fetch('/api/conversations/' + encodeURIComponent(chatId) + '/feedback', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ turn_index: turnIndex, emoji: emoji || '' }),
        });
        if (!r.ok) throw new Error('feedback failed: ' + r.status);
        return r.json();
      },

      loadFeedback: async (chatId) => {
        const r = await fetch('/api/conversations/' + encodeURIComponent(chatId) + '/feedback');
        if (!r.ok) return [];
        return r.json();
      },

      // Artefacts for a chat — returns [{id, kind, title, bytes, created_at, metadata?}].
      listArtefacts: async (chatId) => {
        const r = await fetch('/api/conversations/' + encodeURIComponent(chatId) + '/artefacts');
        if (!r.ok) return [];
        return r.json();
      },

      // Fetch the raw markdown body of an artefact.  Returns the text.
      loadArtefact: async (id) => {
        const r = await fetch('/api/artefacts/' + encodeURIComponent(id));
        if (!r.ok) throw new Error('artefact load failed: ' + r.status);
        return r.text();
      },

      // Send a turn.  `files` is an optional array of File objects
      // (from <input type="file"> or drag-drop).  Each is read into
      // base64 and sent as an attachment — controller dispatches
      // through Agent::run_with_attachments (same path Telegram uses
      // for photos / voice notes / docs).
      //
      // Returns the EventSource (caller can close()).  Callback shape:
      //   onText(delta), onToolStart({id,name}),
      //   onToolResult({content,is_error,view?}), onCheckpoint({text}),
      //   onError(message), onDone().
      send: (id, prompt, cb, files) => {
        cb = cb || {};
        const es = new EventSource('/api/conversations/' + encodeURIComponent(id) + '/events');
        es.onmessage = (ev) => {
          let msg;
          try { msg = JSON.parse(ev.data); } catch { return; }
          switch (msg.type) {
            case 'text':        cb.onText        && cb.onText(msg.delta); break;
            case 'thinking':    cb.onThinking    && cb.onThinking(msg.delta); break;
            case 'tool_start':  cb.onToolStart   && cb.onToolStart(msg); break;
            case 'tool_result': cb.onToolResult  && cb.onToolResult(msg); break;
            case 'checkpoint':  cb.onCheckpoint  && cb.onCheckpoint(msg); break;
            case 'file':        cb.onFile        && cb.onFile(msg); break;
            case 'artefact':    cb.onArtefact    && cb.onArtefact(msg); break;
            case 'llm_error':   cb.onError       && cb.onError(msg.message); break;
            case 'done':        es.close(); cb.onDone && cb.onDone(); break;
          }
        };

        const buildBody = async () => {
          const attachments = [];
          for (const f of (files || [])) {
            const data_base64 = await fileToBase64(f);
            attachments.push({
              name: f.name,
              mime_type: f.type || 'application/octet-stream',
              data_base64,
            });
          }
          return JSON.stringify({ prompt, attachments });
        };

        buildBody().then(body => fetch(
          '/api/conversations/' + encodeURIComponent(id) + '/turn',
          { method: 'POST', headers: { 'Content-Type': 'application/json' }, body },
        )).then(r => {
          if (!r.ok) { es.close(); cb.onError && cb.onError('turn rejected: ' + r.status); }
        }).catch(e => {
          es.close(); cb.onError && cb.onError('turn failed: ' + e.message);
        });
        return es;
      },

      cancel: async (id) => {
        await fetch('/api/conversations/' + encodeURIComponent(id) + '/cancel', { method: 'POST' });
      },
    };

    window.dispatchEvent(new CustomEvent('dyson:live-ready'));
    console.info('[dyson] live mode — connected to HttpController');
  }
})();
