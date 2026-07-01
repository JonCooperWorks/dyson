/* Dyson — primary views: TopBar, LeftRail.
 *
 * Secondary views (Mind / Activity / Artefacts) live in
 * views-secondary.jsx so they can be code-split and lazy-loaded — the
 * cold-paint bundle carries only the conversation shell. */

import React, { useState, useEffect } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { resolvedTheme, toggleTheme } from '../lib/theme.js';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useEscapeKey } from 'dyson-common-ui';
import {
  switchProviderModel, removeConversation, upsertConversation,
} from '../store/app.js';
import { deleteSession } from '../store/sessions.js';

const NAVS = [
  { id: 'conv',      name: 'Conversations', k: '1', icon: 'chat' },
  { id: 'mind',      name: 'Mind',          k: '2', icon: 'brain' },
  { id: 'artefacts', name: 'Artefacts',     k: '3', icon: 'file' },
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
export function brandMark(agentName) {
  const name = brandLabel(agentName);
  return name.replace(/\s+/g, '').slice(0, 1).toUpperCase() || 'D';
}

// Swaps light ⇄ dark.  Bare icon, no label — the glyph is the current theme
// (moon = dark, sun = light); clicking flips to the other.
const THEME_ICON = { light: 'sun', dark: 'moon' };
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
  const totalModels = providers.reduce((n, p) => n + (p.models?.length || 0), 0);

  const [menuOpen, setMenuOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const drawerTitle = view === 'mind'
    ? 'Workspace files'
    : view === 'artefacts'
      ? 'Artefacts'
      : 'Conversations';
  // Active provider starts open, others collapsed.  Resets on menu open
  // so the initial render always matches the current active provider.
  const [expanded, setExpanded] = useState({});
  useEffect(() => {
    if (!menuOpen) return;
    const init = {};
    for (const p of providers) init[p.id] = !!p.active;
    setExpanded(init);
  }, [menuOpen, providers]);

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
      <div className="brand"><div className="mark">{brandMark(agentName)}</div><div className="name">{brandLabel(agentName)}</div></div>
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
                  onClick={() => totalModels > 0 && setMenuOpen(o => !o)}
                  disabled={totalModels === 0}
                  aria-haspopup="menu" aria-expanded={menuOpen}
                  aria-label={totalModels > 0 ? 'Switch model' : 'Active model'}
                  style={{opacity: busy ? 0.5 : 1}}
                  title={totalModels > 0 ? 'Switch model' : 'Active model'}>
            <span className="label">{nextRunModel ? 'next' : 'model'}</span>
            <span className="mono">{nextRunModel ? nextRunModel.model : model}</span>
            {totalModels > 0 && <Icon name="chevd" size={10}/>}
          </button>
        )}
        {menuOpen && totalModels > 0 && (
          <ModelMenu providers={providers} model={model} expanded={expanded}
                     nextRunModel={nextRunModel}
                     onToggleGroup={id => setExpanded(e => ({ ...e, [id]: !e[id] }))}
                     onPick={switchTo} onDismiss={() => setMenuOpen(false)}/>
        )}
      </div>
    </div>
  );
}

function ModelMenu({ providers, model, expanded, nextRunModel, onToggleGroup, onPick, onDismiss }) {
  useEscapeKey(onDismiss);
  return (
    <>
      <div className="modelmenu-scrim" onClick={onDismiss}/>
      <div className="modelmenu">
        {providers.map(p => {
          const open = !!expanded[p.id];
          const models = p.models || [];
          return (
            <div key={p.id} className={`group ${p.active ? 'active' : ''}`}>
              <div className="g-head" onClick={() => onToggleGroup(p.id)}>
                <span className="caret" style={{transform: open ? 'rotate(90deg)' : 'none'}}>
                  <Icon name="chev" size={10}/>
                </span>
                <span className="name">{p.name}</span>
                {p.active && <span className="badge">active</span>}
                <span className="count">{models.length}</span>
              </div>
              {open && models.length > 0 && (
                <div className="g-body">
                  {models.map(m => {
                    const next = nextRunModel?.provider === p.id && nextRunModel?.model === m;
                    const current = p.active && m === model;
                    return (
                    <div key={m}
                         className={`item ${current ? 'on' : ''} ${next ? 'next' : ''}`}
                         onClick={() => onPick(p.id, m)}>
                      <span className="dot"/>
                      <span className="model mono">{m}</span>
                      {next && <span className="badge">next run</span>}
                    </div>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}
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
