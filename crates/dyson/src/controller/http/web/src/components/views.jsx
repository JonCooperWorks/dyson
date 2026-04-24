/* Dyson — primary views: TopBar, LeftRail, RightRail.
 *
 * The secondary views (Mind / Activity / Artefacts) live in
 * views-secondary.jsx so they can be split into their own chunk and
 * lazy-loaded — on cold load the user only pays for the conversation
 * shell, not the full UI. */

import React, { useState, useEffect, Suspense, lazy } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import {
  selectActiveModel, selectProviders, selectConversations, selectTools,
  switchProviderModel, removeConversation, upsertConversation,
} from '../store/app.js';
import { deleteSession } from '../store/sessions.js';

// ToolPanel dispatches to ~8 tool-view renderers (bash/diff/sbom/taint
// /read/image/thinking/fallback).  None of them are invoked on the
// cold-paint route because session.panels starts empty — defer the
// whole module to its own chunk so the initial bundle shrinks by the
// weight of every view renderer that hasn't been asked for yet.
const ToolPanel = lazy(() =>
  import('./panels.jsx').then(m => ({ default: m.ToolPanel })));

function TopBar({ view, setView, onToggleLeft, onToggleRight, rightHidden }) {
  const navs = [
    { id: 'conv',      name: 'Conversations', k: '1', icon: 'chat' },
    { id: 'mind',      name: 'Mind',          k: '2', icon: 'brain' },
    { id: 'artefacts', name: 'Artefacts',     k: '3', icon: 'file' },
    { id: 'activity',  name: 'Activity',      k: '4', icon: 'activity' },
  ];
  const client = useApi();
  const model = useAppState(selectActiveModel);
  const providers = useAppState(selectProviders);
  const totalModels = providers.reduce((n, p) => n + ((p.models && p.models.length) || 0), 0);

  const [menuOpen, setMenuOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  // expanded[providerId] === true → group is open.  Active provider
  // starts open, others collapsed.  Resets each time the menu opens so
  // the initial render matches the current active provider.
  const [expanded, setExpanded] = useState({});
  useEffect(() => {
    if (!menuOpen) return;
    const init = {};
    for (const p of providers) init[p.id] = !!p.active;
    setExpanded(init);
  }, [menuOpen, providers]);

  const toggle = (id) => setExpanded(e => ({ ...e, [id]: !e[id] }));

  const switchTo = async (provider, modelName) => {
    setBusy(true);
    try {
      await client.postModel(provider, modelName);
      switchProviderModel(provider, modelName);
    } catch (e) { console.error(e); }
    setBusy(false);
    setMenuOpen(false);
  };

  return (
    <div className="topbar">
      <button className="menu-toggle" title="Conversations" onClick={onToggleLeft}><Icon name="menu" size={14}/></button>
      <div className="brand"><div className="mark">D</div><div className="name">Dyson</div></div>
      <nav>
        {navs.map(n => (
          <button key={n.id} className={view === n.id ? 'active' : ''} onClick={() => setView(n.id)}
                  aria-label={n.name} aria-current={view === n.id ? 'page' : undefined}>
            <Icon name={n.icon} size={13}/> <span>{n.name}</span> <span className="k">⌘{n.k}</span>
          </button>
        ))}
      </nav>
      <div className="spacer"/>
      <div className="meta" style={{position:'relative'}}>
        {model && (
          <span className="select" onClick={() => totalModels > 0 && setMenuOpen(o => !o)}
                style={{cursor: totalModels > 0 ? 'pointer' : 'default', opacity: busy ? 0.5 : 1}}
                title={totalModels > 0 ? 'Switch model' : 'Active model'}>
            <span className="label">model</span> <span className="mono">{model}</span>
            {totalModels > 0 && <Icon name="chevd" size={10}/>}
          </span>
        )}
        <button className={`menu-toggle ${!rightHidden ? 'active' : ''}`}
                title={rightHidden ? 'Show tool stack' : 'Hide tool stack'}
                onClick={onToggleRight}>
          <Icon name="plug" size={14}/>
        </button>
        {menuOpen && totalModels > 0 && (
          <>
            <div className="modelmenu-scrim" onClick={() => setMenuOpen(false)}/>
            <div className="modelmenu">
              {providers.map(p => {
                const open = !!expanded[p.id];
                const models = p.models || [];
                return (
                  <div key={p.id} className={`group ${p.active ? 'active' : ''}`}>
                    <div className="g-head" onClick={() => toggle(p.id)}>
                      <span className="caret" style={{transform: open ? 'rotate(90deg)' : 'none'}}>
                        <Icon name="chev" size={10}/>
                      </span>
                      <span className="name">{p.name}</span>
                      {p.active && <span className="badge">active</span>}
                      <span className="count">{models.length}</span>
                    </div>
                    {open && models.length > 0 && (
                      <div className="g-body">
                        {models.map(m => (
                          <div key={m}
                               className={`item ${(p.active && m === model) ? 'on' : ''}`}
                               onClick={() => switchTo(p.id, m)}>
                            <span className="dot"/>
                            <span className="model mono">{m}</span>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function LeftRail({ active, setActive, filter, emptyLabel }) {
  // Chat history is shared across controllers; one flat list is the
  // accurate shape (Telegram-originated and HTTP-originated chats both
  // live in ~/.dyson/chats).  `filter` trims the list for views that
  // only care about a subset (e.g. Artefacts hides chats with nothing
  // to read).
  const client = useApi();
  const all = useAppState(selectConversations);
  const items = typeof filter === 'function' ? all.filter(filter) : all;
  const newConv = () => {
    // Don't pass `rotate_previous`: auto-rotating the active chat on
    // every "+ New Conversation" click hollowed out the user's prior
    // transcript (messages went to an archive file they couldn't see
    // without CLI access).  Rotation is opt-in via /clear; explicit
    // removal is via the per-row delete button.
    client.createChat('New conversation').then(c => {
      upsertConversation({ id: c.id, title: c.title, live: false, source: 'http' });
      setActive(c.id);
    }).catch(() => {});
  };
  const deleteConv = (id, e) => {
    // Stop the row's onClick from firing and switching to the chat
    // we're about to remove.
    e.stopPropagation();
    client.deleteChat(id).then(() => {
      removeConversation(id);
      deleteSession(id);
      if (active === id) {
        const list = all.filter(c => c.id !== id);
        const next = list[0];
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
      <div className="search"><input placeholder="Filter conversations"/></div>
      <div className="scroll">
        {items.length === 0 ? (
          <div style={{padding:'18px 14px', color:'var(--mute)', fontSize:12, lineHeight:1.5}}>
            {emptyLabel || <>No conversations yet. <span className="mono" style={{color:'var(--fg-dim)'}}>⌘K</span> to start one.</>}
          </div>
        ) : (
          <div className="group">
            <h4>Conversations <span className="n">· {items.length}</span></h4>
            {items.map(c => (
              <div key={c.id} className={`conv ${c.live ? 'live' : ''} ${active === c.id ? 'active' : ''} src-${c.source || 'http'}`}
                   onClick={() => setActive(c.id)}>
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
                  <button className="conv-del" title="Delete conversation"
                          onClick={(e) => deleteConv(c.id, e)}>
                    <Icon name="x" size={11}/>
                  </button>
                </div>
                <div className="row2">
                  <span className="last mono" style={{fontSize:10.5, opacity:0.6}}>{c.id}</span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </aside>
  );
}

function RightRail({ panels, onClose, activeChatId }) {
  const tools = useAppState(selectTools);
  const client = useApi();
  // Poll /api/activity so the Tool Stack surfaces any running subagent
  // for this chat even when the user hasn't clicked the chip to open
  // its panel.  Scoped to the active chat — stack-wide lists live in
  // the Activity tab.  Refresh cadence matches ActivityView (3s).
  const [runningSubagents, setRunningSubagents] = useState([]);
  useEffect(() => {
    if (!activeChatId) { setRunningSubagents([]); return; }
    let cancelled = false;
    const refresh = () => {
      client.getActivity(activeChatId)
        .then(j => {
          if (cancelled || !j) return;
          const running = (j.lanes || []).filter(a => a.status === 'running');
          setRunningSubagents(running);
        })
        .catch(() => {});
    };
    refresh();
    const id = setInterval(() => { if (!document.hidden) refresh(); }, 3000);
    return () => { cancelled = true; clearInterval(id); };
  }, [activeChatId, client]);
  const pulseCount = runningSubagents.length;
  return (
    <aside className="right">
      <div className="r-head">
        <span className="title">Tool stack</span>
        <span className="count">{panels.length}</span>
        <div className="spacer"/>
      </div>
      <div className="r-stack">
        {pulseCount > 0 && (
          <div className="r-section">
            <div className="r-section-head">
              <span>Running</span>
              <span className="count mono">{pulseCount}</span>
            </div>
            <div className="r-running">
              {runningSubagents.map((a, i) => (
                <div key={i} className="r-running-row" title={a.note}>
                  <span className="dot running"/>
                  <span className="name mono">{a.name}</span>
                  <span className="note">{a.note}</span>
                </div>
              ))}
            </div>
          </div>
        )}
        {panels.length === 0 && pulseCount === 0 && (
          <div style={{color:'var(--mute)', fontSize:12, padding:24, textAlign:'center', lineHeight:1.5}}>
            Tool panels appear here when Dyson runs tools.
            Click <span className="mono">[open]</span> on a tool chip in the transcript.
          </div>
        )}
        {panels.length > 0 && (
          <Suspense fallback={<div/>}>
            {panels.map(ref => {
              const t = tools[ref];
              if (!t) return null;
              return <ToolPanel key={ref} tool={t} onClose={() => onClose(ref)}/>;
            })}
          </Suspense>
        )}
      </div>
    </aside>
  );
}

export { TopBar, LeftRail, RightRail };
