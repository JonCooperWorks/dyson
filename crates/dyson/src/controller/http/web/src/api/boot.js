/* Dyson — cold-load + polling loop.
 *
 * Probes /api/conversations first.  On success flips `live: true`,
 * fires the rest of the cold-load fetches in parallel, and installs a
 * 10s polling loop on conversations so Telegram-originated chats
 * surface in the sidebar without a cross-controller push channel.
 * Returns a disposer; tests stop the interval between runs.
 */

import {
  setLive, setConversations, setProviders, setMind, setActivity, setAgentName,
} from '../store/app.js';

// Pull the user-facing agent name out of IDENTITY.md.  swarm.rs writes
// "Name: <name>" on instance create; users editing the file in the Mind
// view follow the same convention.  Returns '' when no Name line is
// present so the caller's fallback to "dyson" kicks in.
function parseAgentName(md) {
  if (typeof md !== 'string') return '';
  for (const line of md.split('\n')) {
    const m = /^\s*Name\s*:\s*(.+?)\s*$/i.exec(line);
    if (m) return m[1];
  }
  return '';
}

const toConvRow = (c) => ({
  id: c.id, title: c.title,
  live: !!c.live, hasArtefacts: !!c.has_artefacts,
  source: c.source || 'http',
});

export function boot(client, { pollMs = 10_000, doc = (typeof document !== 'undefined' ? document : null) } = {}) {
  let disposed = false;
  let intervalId = null;

  client.listConversations().then(async list => {
    if (disposed) return;
    setLive(true);
    // First-login UX: if the user has no conversations yet, mint one
    // so the chat surface is immediately usable.  Without this, the
    // SPA renders an empty chat pane with no clear next step (the
    // "+ New Conversation" button is in the sidebar drawer that's
    // collapsed by default on mobile).
    if (Array.isArray(list) && list.length === 0) {
      try {
        const fresh = await client.createChat('New conversation');
        if (!disposed && fresh && fresh.id) list = [fresh];
      } catch (e) {
        // Best-effort — empty list is still better than crashing the boot.
        console.info('[dyson] auto-create initial conversation failed', e);
      }
    }
    setConversations(list.map(toConvRow));

    client.listProviders().then(provs => {
      if (disposed) return;
      const providers = (provs || []).map(p => ({
        id: p.id, name: p.name, models: p.models || [],
        activeModel: p.active_model || '', active: !!p.active,
      }));
      setProviders(providers, providers[0]?.activeModel || '');
    }).catch(() => {});

    client.getMind().then(m => {
      if (disposed) return;
      setMind({
        backend: m.backend || '',
        files: (m.files || []).map(f => ({
          path: f.path,
          size: typeof f.size === 'number' ? `${f.size} B` : '',
        })),
        open: { path: '', content: '' },
      });
      const hasIdentity = (m.files || []).some(f => f.path === 'IDENTITY.md');
      if (hasIdentity) {
        client.mindFile('IDENTITY.md').then(file => {
          if (disposed) return;
          setAgentName(parseAgentName(file?.content || ''));
        }).catch(() => {});
      }
    }).catch(() => {});

    client.getActivity().then(act => {
      if (!disposed) setActivity(act.lanes || []);
    }).catch(() => {});

    intervalId = setInterval(() => {
      if (disposed || doc?.hidden) return;
      client.listConversations().then(next => {
        if (!Array.isArray(next) || disposed) return;
        setConversations(next.map(toConvRow));
      }).catch(() => {});
    }, pollMs);
  }).catch(() => {
    // Controller unreachable — the shell renders with empty lists.
    console.info('[dyson] live API not reachable — staying in cold mode');
  });

  return () => {
    disposed = true;
    if (intervalId) { clearInterval(intervalId); intervalId = null; }
  };
}
