/* Dyson — primary views: TopBar, LeftRail, RightRail.
 *
 * Secondary views (Mind / Activity / Artefacts) live in
 * views-secondary.jsx so they can be code-split and lazy-loaded — the
 * cold-paint bundle carries only the conversation shell. */

import React, { useState, useEffect, useRef, useLayoutEffect, Suspense, lazy } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useSession } from '../hooks/useSession.js';
import {
  switchProviderModel, removeConversation, upsertConversation,
} from '../store/app.js';
import { deleteSession, updateSession, closePanel } from '../store/sessions.js';

// ToolPanel dispatches to ~8 tool-view renderers (bash/diff/sbom/taint/
// read/image/thinking/fallback).  None are invoked on the cold-paint
// route because session.panels starts empty — defer the whole module
// so the initial bundle shrinks by the weight of every renderer that
// hasn't been asked for yet.
const ToolPanel = lazy(() =>
  import('./panels.jsx').then(m => ({ default: m.ToolPanel })));

const NAVS = [
  { id: 'conv',      name: 'Conversations', k: '1', icon: 'chat' },
  { id: 'mind',      name: 'Mind',          k: '2', icon: 'brain' },
  { id: 'artefacts', name: 'Artefacts',     k: '3', icon: 'file' },
  { id: 'activity',  name: 'Activity',      k: '4', icon: 'activity' },
];

function TopBar({ view, setView, onToggleLeft, onToggleRight, rightHidden }) {
  const client = useApi();
  const model = useAppState(s => s.activeModel);
  const providers = useAppState(s => s.providers);
  const totalModels = providers.reduce((n, p) => n + (p.models?.length || 0), 0);

  const [menuOpen, setMenuOpen] = useState(false);
  const [busy, setBusy] = useState(false);
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
      await client.postModel(provider, modelName);
      switchProviderModel(provider, modelName);
    } catch (e) { console.error(e); }
    setBusy(false);
    setMenuOpen(false);
  };

  return (
    <div className="topbar">
      <button className="menu-toggle" title="Conversations" onClick={onToggleLeft}>
        <Icon name="menu" size={14}/>
      </button>
      <div className="brand"><div className="mark">D</div><div className="name">Dyson</div></div>
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
          <ModelMenu providers={providers} model={model} expanded={expanded}
                     onToggleGroup={id => setExpanded(e => ({ ...e, [id]: !e[id] }))}
                     onPick={switchTo} onDismiss={() => setMenuOpen(false)}/>
        )}
      </div>
    </div>
  );
}

function ModelMenu({ providers, model, expanded, onToggleGroup, onPick, onDismiss }) {
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
                  {models.map(m => (
                    <div key={m}
                         className={`item ${(p.active && m === model) ? 'on' : ''}`}
                         onClick={() => onPick(p.id, m)}>
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
  );
}

function LeftRail({ active, setActive, filter, emptyLabel }) {
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
  const newConv = () => client.createChat('New conversation').then(c => {
    upsertConversation({ id: c.id, title: c.title, live: false, source: 'http' });
    setActive(c.id);
  }).catch(() => {});

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

// RightRail pulls its own session slice.  The active chat id is the
// only prop — everything else (panels, panel close, tools dict) flows
// through the store.
function RightRail({ chatId }) {
  const session = useSession(chatId);
  const tools = useAppState(s => s.tools);
  const panels = session?.panels || [];
  const openTool = session?.openTool || null;
  const runningSubagents = useRunningSubagents(chatId);
  const pulseCount = runningSubagents.length;
  const stackRef = useRef(null);

  const onClose = (ref) => updateSession(chatId, s => closePanel(s, ref));

  // Clicking a tool chip (or restoring `#/c/<id>/t/<ref>` from the URL)
  // updates `session.openTool` — the stack already renders the panel,
  // but if it's offscreen below the rail's fold the user sees nothing
  // happen. Scroll the matching panel into view whenever openTool flips
  // so the click-to-open feels like an actual jump. `block: 'nearest'`
  // avoids yanking the rail when the panel is already visible. The
  // requestAnimationFrame round-trip is for the lazy ToolPanel module:
  // first render after Suspense resolves may not have the DOM node yet.
  useLayoutEffect(() => {
    if (!openTool || !stackRef.current) return;
    const find = () => stackRef.current?.querySelector(
      `[data-tool-ref="${cssEscape(openTool)}"]`);
    const node = find();
    if (node) { node.scrollIntoView({ block: 'nearest' }); return; }
    const raf = requestAnimationFrame(() => {
      const late = find();
      if (late) late.scrollIntoView({ block: 'nearest' });
    });
    return () => cancelAnimationFrame(raf);
  }, [openTool, panels.length]);

  return (
    <aside className="right">
      <div className="r-head">
        <span className="title">Tool stack</span>
        <span className="count">{panels.length}</span>
        <div className="spacer"/>
      </div>
      <div className="r-stack" ref={stackRef}>
        {pulseCount > 0 && <RunningSection agents={runningSubagents}/>}
        {panels.length === 0 && pulseCount === 0 && (
          <div style={{color:'var(--mute)', fontSize:12, padding:24, textAlign:'center', lineHeight:1.5}}>
            Tool panels appear here when Dyson runs tools.
            Click <span className="mono">[open]</span> on a tool chip in the transcript.
          </div>
        )}
        {panels.length > 0 && (
          <Suspense fallback={<div/>}>
            {panels.map(ref => tools[ref] && (
              <ToolPanel key={ref} toolRef={ref} tool={tools[ref]} onClose={() => onClose(ref)}/>
            ))}
          </Suspense>
        )}
      </div>
    </aside>
  );
}

// Tool refs are mint-generated (`${chatId}-${kind}-${counter}`) so they
// can't contain quotes or backslashes today, but a tool id arriving over
// SSE could theoretically. CSS.escape is the standards-compliant escape;
// fall back to a hand-rolled minimal escape for older runtimes (jsdom in
// some test setups).
function cssEscape(s) {
  if (typeof CSS !== 'undefined' && typeof CSS.escape === 'function') return CSS.escape(s);
  return String(s).replace(/["\\]/g, '\\$&');
}

// Poll /api/activity for this chat so the Tool Stack surfaces any
// running subagent even when the user hasn't clicked a chip to open
// its panel.  3s cadence matches ActivityView.
function useRunningSubagents(chatId) {
  const client = useApi();
  const [running, setRunning] = useState([]);
  useEffect(() => {
    if (!chatId) { setRunning([]); return; }
    let alive = true;
    const refresh = () => client.getActivity(chatId).then(j => {
      if (!alive || !j) return;
      setRunning((j.lanes || []).filter(a => a.status === 'running'));
    }).catch(() => {});
    refresh();
    const id = setInterval(() => { if (!document.hidden) refresh(); }, 3000);
    return () => { alive = false; clearInterval(id); };
  }, [chatId, client]);
  return running;
}

function RunningSection({ agents }) {
  return (
    <div className="r-section">
      <div className="r-section-head">
        <span>Running</span>
        <span className="count mono">{agents.length}</span>
      </div>
      <div className="r-running">
        {agents.map((a, i) => (
          <div key={i} className="r-running-row" title={a.note}>
            <span className="dot running"/>
            <span className="name mono">{a.name}</span>
            <span className="note">{a.note}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

export { TopBar, LeftRail, RightRail };
