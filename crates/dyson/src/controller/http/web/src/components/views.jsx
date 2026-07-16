/* Dyson — primary views: TopBar, LeftRail.
 *
 * Secondary views (Mind / Activity / Artefacts) live in
 * views-secondary.jsx so they can be code-split and lazy-loaded — the
 * cold-paint bundle carries only the conversation shell. */

import React, { useState, useEffect } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { DysonMark, createThemeController } from 'dyson-common-ui';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useEscapeKey } from 'dyson-common-ui';
import {
  switchProviderModel, removeConversation, upsertConversation,
} from '../store/app.js';
import { deleteSession } from '../store/sessions.js';

// Theme controller bound to the swarm-shared cookie: `stripInstanceLabel`
// scopes it to the swarm host so a dyson subdomain follows the same choice
// as the apex + sibling dysons.  Destructured (not held as `theme`) to dodge
// the local `theme` state var in ThemeToggle below.
const { resolvedTheme, toggleTheme } = createThemeController({ storageKey: 'dyson-theme', stripInstanceLabel: true });

const NAVS = [
  { id: 'conv',      name: 'Conversations', k: '1', icon: 'chat' },
  { id: 'mind',      name: 'Mind',          k: '2', icon: 'brain' },
  { id: 'artefacts', name: 'Artifacts',     k: '3', icon: 'file' },
  { id: 'activity',  name: 'Activity',      k: '4', icon: 'activity' },
  { id: 'audit',     name: 'Audit',         k: '5', icon: 'gauge' },
];

// Brand label + initial pulled from the swarm-set agent name (lives in
// IDENTITY.md `Name:`, populated by SWARM_NAME).  Falls back to "Dyson"
// when the agent hasn't been named yet — same fallback the per-turn
// header uses.  Exported for tests so the fallback rule stays pinned.
export function brandLabel(agentName) {
  return (agentName || '').trim() || 'Dyson';
}

// Swaps light ⇄ dark.  Bare icon, no label — the glyph shows the theme a
// click switches TO (moon while light, sun while dark).
const THEME_ICON = { light: 'moon', dark: 'sun' };
function ThemeToggle() {
  const [theme, setTheme] = useState(resolvedTheme);
  const label = `Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`;
  return (
    <button type="button" className="btn ghost icon sm" title={label} aria-label={label}
            onClick={() => setTheme(toggleTheme())}>
      <Icon name={THEME_ICON[theme]} size={15}/>
    </button>
  );
}

function TopBar({ view, setView, onToggleLeft, onNewChat, running, nextRunModel, onPickModel }) {
  const client = useApi();
  const model = useAppState(s => s.activeModel);
  const providers = useAppState(s => s.providers);
  const agentName = useAppState(s => s.agentName);
  // The active provider a catalogue pick switches to.  Falls back to the
  // first provider when none is flagged active (fresh boot).
  const activeProvider = (providers.find(p => p.active) || providers[0])?.id || '';
  // The models named in dyson.json — shown as the "current" group so the
  // seeded/active model is always one click away without a catalogue fetch.
  const configured = providers.flatMap(p =>
    (p.models || []).map(m => ({ provider: p.id, id: m, active: !!p.active && m === model }))
  );
  // There's always something to switch *from* once a model is active, and
  // the catalogue is fetched lazily on open — so the control is enabled
  // whenever a model is set, not gated on the (now single-entry) configured
  // list.
  const canSwitch = !!model;

  const [menuOpen, setMenuOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  // Catalogue is null until the menu is first opened, then the normalized
  // list from GET /api/models (or [] off-swarm / on error).
  const [catalogue, setCatalogue] = useState(null);
  const [cataLoading, setCataLoading] = useState(false);
  const drawerTitle = view === 'mind'
    ? 'Workspace files'
    : view === 'artefacts'
      ? 'Artifacts'
      : 'Conversations';

  // Fetch the full catalogue lazily the first time the menu opens.  Older
  // test/embed clients may not expose listModels — degrade to the
  // configured list rather than throwing.
  useEffect(() => {
    if (!menuOpen || catalogue !== null || typeof client.listModels !== 'function') return;
    let alive = true;
    setCataLoading(true);
    client.listModels()
      .then(r => { if (alive) setCatalogue(Array.isArray(r?.models) ? r.models : []); })
      .catch(() => { if (alive) setCatalogue([]); })
      .finally(() => { if (alive) setCataLoading(false); });
    return () => { alive = false; };
  }, [menuOpen, catalogue, client]);

  const switchTo = async (provider, modelName) => {
    setBusy(true);
    try {
      if (typeof onPickModel === 'function') {
        await onPickModel(provider, modelName);
      } else {
        await client.postModel(provider, modelName);
        switchProviderModel(provider, modelName);
      }
    } catch (e) { console.error(e); }
    setBusy(false);
    setMenuOpen(false);
  };

  return (
    <div className="topbar">
      <button className="menu-toggle" title={drawerTitle} aria-label={drawerTitle} onClick={onToggleLeft}>
        <Icon name="menu" size={14}/>
      </button>
      {typeof onNewChat === 'function' && (
        <button className="new-chat" title="New conversation" aria-label="New conversation" onClick={onNewChat}>
          <Icon name="compose" size={15}/>
        </button>
      )}
      <div className="brand"><DysonMark className="brand-logo" size={22} aria-hidden="true"/><div className="name">{brandLabel(agentName)}</div></div>
      <nav>
        {NAVS.map(n => (
          <button key={n.id} className={view === n.id ? 'active' : ''} onClick={() => setView(n.id)}
                  aria-label={n.name} aria-current={view === n.id ? 'page' : undefined}>
            <Icon name={n.icon} size={13}/> <span>{n.name}</span> <span className="k">⌘{n.k}</span>
          </button>
        ))}
      </nav>
      <div className="spacer"/>
      <div className="meta" style={{position:'relative'}}>
        <ThemeToggle/>
        {model && (
          <button type="button" className="select"
                  onClick={() => canSwitch && setMenuOpen(o => !o)}
                  disabled={!canSwitch}
                  aria-haspopup="menu" aria-expanded={menuOpen}
                  aria-label={canSwitch ? 'Switch model' : 'Active model'}
                  style={{opacity: busy ? 0.5 : 1}}
                  title={canSwitch ? 'Switch model' : 'Active model'}>
            <span className="label">{nextRunModel ? 'next' : 'model'}</span>
            <span className="mono">{nextRunModel ? nextRunModel.model : model}</span>
            {canSwitch && <Icon name="chevd" size={10}/>}
          </button>
        )}
        {menuOpen && canSwitch && (
          <ModelMenu configured={configured} catalogue={catalogue} loading={cataLoading}
                     activeProvider={activeProvider} activeModel={model}
                     nextRunModel={nextRunModel}
                     onPick={switchTo} onDismiss={() => setMenuOpen(false)}/>
        )}
      </div>
    </div>
  );
}

// Compact context-window label: 200000 → "200K", 1048576 → "1M".
function fmtCtx(n) {
  if (!n || n <= 0) return '';
  if (n >= 1_000_000) return `${+(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}K`;
  return String(n);
}

// Searchable picker over the full model catalogue (GET /api/models),
// distinct from the old provider-tree menu that only listed the models
// named in dyson.json.  A small "current" group keeps the configured /
// active model one click away; the rest of the list is type-to-filter
// over the catalogue, bounded so a 300-model list never renders whole.
const CATALOGUE_LIMIT = 30;
function ModelMenu({ configured, catalogue, loading, activeProvider, activeModel, nextRunModel, onPick, onDismiss }) {
  useEscapeKey(onDismiss);
  const [query, setQuery] = useState('');
  const q = query.trim().toLowerCase();

  const configuredIds = new Set(configured.map(c => c.id));
  const cata = Array.isArray(catalogue) ? catalogue : [];
  // Hide configured ids from the catalogue list — they already show in the
  // "current" group, and duplicating them just pads the results.
  const matches = cata.filter(m => {
    if (configuredIds.has(m.id)) return false;
    if (!q) return true;
    return (m.id || '').toLowerCase().includes(q)
      || (m.name || '').toLowerCase().includes(q);
  });
  const shown = matches.slice(0, CATALOGUE_LIMIT);
  const overflow = matches.length - shown.length;

  return (
    <>
      <div className="modelmenu-scrim" onClick={onDismiss}/>
      <div className="modelmenu">
        <div className="mm-search">
          <Icon name="search" size={13}/>
          <input autoFocus placeholder="Search models" value={query}
                 aria-label="Search models"
                 onChange={e => setQuery(e.target.value)}/>
        </div>

        {configured.length > 0 && !q && (
          <div className="mm-section">
            <div className="mm-label">Current</div>
            {configured.map(c => {
              const next = nextRunModel?.provider === c.provider && nextRunModel?.model === c.id;
              return (
                <div key={`${c.provider}/${c.id}`}
                     className={`item ${c.active ? 'on' : ''} ${next ? 'next' : ''}`}
                     onClick={() => onPick(c.provider, c.id)}>
                  <span className="dot"/>
                  <span className="model mono">{c.id}</span>
                  {next && <span className="badge">next run</span>}
                </div>
              );
            })}
          </div>
        )}

        <div className="mm-section">
          <div className="mm-label">Catalogue{loading ? ' · loading' : ''}</div>
          {!loading && shown.length === 0 && (
            <div className="mm-empty">
              {cata.length === 0 ? 'No catalogue available.' : 'No matches.'}
            </div>
          )}
          {shown.map(m => {
            const next = nextRunModel?.provider === activeProvider && nextRunModel?.model === m.id;
            const current = !!activeProvider && m.id === activeModel;
            const ctx = fmtCtx(m.context_length);
            return (
              <div key={m.id}
                   className={`item ${current ? 'on' : ''} ${next ? 'next' : ''}`}
                   onClick={() => onPick(activeProvider, m.id)}
                   title={m.name || m.id}>
                <span className="dot"/>
                <span className="model mono">{m.id}</span>
                {ctx && <span className="mm-ctx">{ctx}</span>}
              </div>
            );
          })}
          {overflow > 0 && (
            <div className="mm-hint">+{overflow} more — keep typing to narrow</div>
          )}
        </div>
      </div>
    </>
  );
}

function LeftRail({ active, setActive, filter, emptyLabel, onNew }) {
  // HTTP + Telegram chats live in the same ~/.dyson/chats directory —
  // one flat list is the accurate shape.  `filter` trims the list for
  // views that only care about a subset (e.g. Artefacts hides chats
  // with nothing to read).
  const client = useApi();
  const all = useAppState(s => s.conversations);
  const [query, setQuery] = useState('');
  const q = query.trim().toLowerCase();
  const matchesQuery = (c) => !q
    || (c.title || '').toLowerCase().includes(q)
    || (c.id || '').toLowerCase().includes(q);
  const items = (typeof filter === 'function' ? all.filter(filter) : all)
    .filter(matchesQuery);

  // Don't auto-rotate on "+ New Conversation" — that once hollowed out
  // the active chat into an archive file the user couldn't see without
  // CLI access.  Rotation is opt-in via /clear; explicit removal is via
  // the per-row delete button.
  // App lifts a single new-conversation handler so the TopBar's mobile
  // new-chat button, the ⌘K shortcut, and this rail button all share one
  // behaviour.  Fall back to a local impl when rendered standalone (tests).
  const newConv = onNew || (() => client.createChat('New conversation').then(c => {
    upsertConversation({ id: c.id, title: c.title, live: false, source: 'http' });
    setActive(c.id);
  }).catch(() => {}));

  const deleteConv = (id, e) => {
    e.stopPropagation();
    client.deleteChat(id).then(() => {
      removeConversation(id);
      deleteSession(id);
      if (active === id) {
        const next = all.find(c => c.id !== id);
        setActive(next ? next.id : null);
      }
    }).catch(() => {});
  };

  return (
    <aside className="left">
      <div className="newc">
        <button className="btn primary" onClick={newConv}>
          <span><Icon name="plus" size={12}/> New conversation</span>
          <Kbd>⌘K</Kbd>
        </button>
      </div>
      <div className="search">
        <input placeholder="Filter conversations"
               value={query}
               onChange={e => setQuery(e.target.value)}/>
      </div>
      <div className="scroll">
        {items.length === 0 ? (
          <div style={{padding:'18px 14px', color:'var(--mute)', fontSize:12, lineHeight:1.5}}>
            {emptyLabel || <>No conversations yet. <span className="mono" style={{color:'var(--fg-dim)'}}>⌘K</span> to start one.</>}
          </div>
        ) : (
          <div className="group">
            <h4>Conversations <span className="n">· {items.length}</span></h4>
            {items.map(c => (
              <ConvRow key={c.id} c={c} active={active === c.id}
                       onOpen={() => setActive(c.id)}
                       onDelete={(e) => deleteConv(c.id, e)}/>
            ))}
          </div>
        )}
      </div>
    </aside>
  );
}

function ConvRow({ c, active, onOpen, onDelete }) {
  return (
    <div className={`conv ${c.live ? 'live' : ''} ${active ? 'active' : ''} src-${c.source || 'http'}`}
         onClick={onOpen}>
      <div className="row1">
        <span className="title">{c.title || c.id}</span>
        {c.source === 'telegram' && (
          <span className="chip tg" title="Telegram-originated chat"
                style={{fontSize:9, padding:'1px 5px', marginRight:4,
                        background:'#229ED9', color:'#fff', borderRadius:3,
                        letterSpacing:0.3, textTransform:'uppercase', fontWeight:600}}>
            TG
          </span>
        )}
        <button className="conv-del" title="Delete conversation" onClick={onDelete}>
          <Icon name="x" size={11}/>
        </button>
      </div>
      <div className="row2">
        <span className="last mono" style={{fontSize:10.5, opacity:0.6}}>{c.id}</span>
      </div>
    </div>
  );
}

export { TopBar, LeftRail, NAVS };
