/* Dyson — initial bootstrap + polling.
 *
 * Probes /api/conversations first to see if the HttpController is
 * reachable; on success flips `live: true` and fires the rest of the
 * cold-load fetches (providers, mind, activity) in parallel.  Installs
 * a 10 s polling loop on conversations so Telegram-originated chats
 * surface in the sidebar without a cross-controller push channel.
 *
 * Returns a disposer — used by tests to stop the interval between runs.
 * In production the web client lives for the tab's lifetime, so the
 * disposer is rarely needed.
 */

import {
  setLive,
  setConversations,
  setProviders,
  setMind,
  setActivity,
} from '../store/app.js';

const toConvRow = (c) => ({
  id: c.id,
  title: c.title,
  live: !!c.live,
  hasArtefacts: !!c.has_artefacts,
  source: c.source || 'http',
});

const toProvider = (p) => ({
  id: p.id,
  name: p.name,
  models: p.models || [],
  activeModel: p.active_model || '',
  active: !!p.active,
});

const toMind = (m) => ({
  backend: m.backend || '',
  files: (m.files || []).map(f => ({
    path: f.path,
    size: typeof f.size === 'number' ? `${f.size} B` : '',
  })),
  open: { path: '', content: '' },
});

export function boot(client, { pollMs = 10_000, doc = (typeof document !== 'undefined' ? document : null) } = {}) {
  let intervalId = null;
  let disposed = false;

  client.listConversations()
    .then(list => {
      if (disposed) return;
      setLive(true);
      setConversations(list.map(toConvRow));

      client.listProviders().then(provs => {
        if (disposed) return;
        const providers = (provs || []).map(toProvider);
        const activeModel = (providers[0] && providers[0].activeModel) || '';
        setProviders(providers, activeModel);
      }).catch(() => {});

      client.getMind().then(mind => {
        if (disposed) return;
        setMind(toMind(mind));
      }).catch(() => {});

      client.getActivity().then(act => {
        if (disposed) return;
        setActivity(act.lanes || []);
      }).catch(() => {});

      intervalId = setInterval(() => {
        if (disposed) return;
        if (doc && doc.hidden) return;
        client.listConversations().then(next => {
          if (!Array.isArray(next) || disposed) return;
          setConversations(next.map(toConvRow));
        }).catch(() => {});
      }, pollMs);
    })
    .catch(() => {
      // Controller unreachable — the shell renders with empty lists.
      // Matches the old bridge.js cold-load behaviour.
      console.info('[dyson] live API not reachable — staying in cold mode');
    });

  return () => {
    disposed = true;
    if (intervalId) { clearInterval(intervalId); intervalId = null; }
  };
}
