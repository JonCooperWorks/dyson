/* Dyson — TopBar / LeftRail / MindView / ActivityView. */

import React, { useState, useEffect, useRef } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { ToolPanel } from './panels.jsx';
import { ArtefactBlock, markdown, prettySize } from './turns.jsx';

function TopBar({ view, setView, onToggleLeft, onToggleRight, rightHidden }) {
  const navs = [
    { id: 'conv',      name: 'Conversations', k: '1', icon: 'chat' },
    { id: 'mind',      name: 'Mind',          k: '2', icon: 'brain' },
    { id: 'artefacts', name: 'Artefacts',     k: '3', icon: 'file' },
    { id: 'activity',  name: 'Activity',      k: '4', icon: 'activity' },
  ];
  const D = window.DYSON_DATA || {};
  const model = D.activeModel || '';
  const providers = D.providers || [];
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
  }, [menuOpen]);

  const toggle = (id) => setExpanded(e => ({ ...e, [id]: !e[id] }));

  const switchTo = async (provider, modelName) => {
    if (!window.DysonLive) { setMenuOpen(false); return; }
    setBusy(true);
    try {
      const r = await fetch('/api/model', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ provider, model: modelName }),
      });
      if (r.ok) {
        D.activeModel = modelName;
        D.providers = providers.map(p => ({
          ...p,
          active: p.id === provider,
          activeModel: p.id === provider ? modelName : p.activeModel,
        }));
        window.dispatchEvent(new CustomEvent('dyson:live-update'));
      }
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
          <button key={n.id} className={view === n.id ? 'active' : ''} onClick={() => setView(n.id)}>
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
  // live in ~/.dyson/chats and the controller has no honest way to
  // attribute origin without metadata that doesn't exist yet).
  // `filter` trims the list for views that only care about a subset
  // (e.g. Artefacts hides chats with nothing to read).
  const all = (window.DYSON_DATA.conversations.http) || [];
  const items = typeof filter === 'function' ? all.filter(filter) : all;
  const newConv = () => {
    if (!window.DysonLive) return;
    // Don't pass `rotate_previous`: auto-rotating the active chat on
    // every "+ New Conversation" click hollowed out the user's prior
    // transcript (messages went to an archive file they couldn't see
    // without CLI access).  Rotation is opt-in via /clear; explicit
    // removal is via the per-row delete button.
    window.DysonLive.createChat('New conversation').then(c => {
      window.DYSON_DATA.conversations.http.unshift({ id: c.id, title: c.title, live: false });
      setActive(c.id);
    });
  };
  const deleteConv = (id, e) => {
    // Stop the row's onClick from firing and switching to the chat
    // we're about to remove.
    e.stopPropagation();
    if (!window.DysonLive) return;
    window.DysonLive.deleteChat(id).then(() => {
      const list = window.DYSON_DATA.conversations.http;
      const idx = list.findIndex(c => c.id === id);
      if (idx !== -1) list.splice(idx, 1);
      if (active === id) {
        // Jump to the next chat (or null if none left) so the main
        // pane doesn't keep showing a tab that no longer exists.
        const next = list[0];
        setActive(next ? next.id : null);
      } else {
        window.dispatchEvent(new CustomEvent('dyson:live-update'));
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

function MindView({ showSide, onHideSide }) {
  const m = window.DYSON_DATA.mind;
  const initial = (m.files[0] && m.files[0].path) || '';
  const [selected, setSelected] = useState(initial);
  const [loaded, setLoaded] = useState('');
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState('');

  useEffect(() => {
    if (!selected) { setLoaded(''); setDraft(''); return; }
    if (window.DysonLive) {
      window.DysonLive.mindFile(selected)
        .then(file => { const c = file.content || ''; setLoaded(c); setDraft(c); setErr(''); })
        .catch(e => { setErr(String(e.message || e)); setLoaded(''); setDraft(''); });
    }
  }, [selected]);

  const dirty = draft !== loaded;
  const save = async () => {
    if (!selected || !window.DysonLive) return;
    setSaving(true); setErr('');
    try {
      const r = await fetch('/api/mind/file', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: selected, content: draft }),
      });
      if (!r.ok) throw new Error('save failed: ' + r.status);
      setLoaded(draft);
    } catch (e) { setErr(String(e.message || e)); }
    setSaving(false);
  };

  useEffect(() => {
    const h = (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 's') {
        e.preventDefault();
        if (dirty && !saving) save();
      }
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [dirty, saving, draft, selected]);

  return (
    <div className={`mind${showSide ? ' show-side' : ''}`}>
      <aside className="mind-side">
        <div style={{padding:'10px 14px', borderBottom:'1px solid var(--line)'}}>
          <div className="eyebrow">workspace</div>
          {m.backend && <div style={{fontSize:13, color:'var(--fg)', marginTop:4}}><span className="mono">{m.backend}</span> backend</div>}
        </div>
        <div style={{overflowY:'auto', flex:1, padding:'6px 0'}}>
          {(m.files.length === 0) && <div style={{padding:'14px', color:'var(--mute)', fontSize:12}}>No workspace files.</div>}
          {m.files.map(f => (
            <div key={f.path} onClick={() => { setSelected(f.path); onHideSide && onHideSide(); }}
                 style={{display:'flex', alignItems:'center', gap:8, padding:'6px 14px', cursor:'pointer',
                         background: selected === f.path ? 'var(--panel)' : 'transparent',
                         borderLeft: selected === f.path ? '2px solid var(--accent)' : '2px solid transparent'}}>
              <Icon name="file" size={11} style={{color:'var(--mute)'}}/>
              <span className="mono" style={{fontSize:12, color:'var(--fg-dim)', flex:1, whiteSpace:'nowrap', overflow:'hidden', textOverflow:'ellipsis'}}>{f.path}</span>
              {f.size && <span className="mono" style={{fontSize:10, color:'var(--mute)'}}>{f.size}</span>}
            </div>
          ))}
        </div>
      </aside>
      <section className="mind-pane">
        <div style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px', borderBottom:'1px solid var(--line)', background:'var(--bg)', flexWrap:'wrap'}}>
          <span className="mono" style={{fontSize:13, color:'var(--fg)'}}>{selected || '—'}</span>
          {dirty && <span className="chip" style={{color:'var(--warn)'}}>unsaved</span>}
          {err && <span className="chip" style={{color:'var(--err)'}}>{err}</span>}
          <span style={{flex:1}}/>
          {dirty && <button className="btn sm ghost" onClick={() => setDraft(loaded)} disabled={saving}>revert</button>}
          <button className="btn sm primary" onClick={save} disabled={!dirty || saving || !selected || !window.DysonLive}>
            {saving ? 'saving…' : 'save'} <Kbd>⌘S</Kbd>
          </button>
        </div>
        <textarea className="mind-editor"
          value={draft}
          onChange={e => setDraft(e.target.value)}
          placeholder={selected ? '(empty)' : 'select a file to edit'}
          spellCheck={false}
          disabled={!selected}/>
      </section>
    </div>
  );
}

function RightRail({ panels, onClose, activeChatId }) {
  const tools = window.DYSON_DATA.tools || {};
  // Poll /api/activity so the Tool Stack surfaces any running subagent
  // for this chat even when the user hasn't clicked the chip to open
  // its panel.  Scoped to the active chat — stack-wide lists live in
  // the Activity tab.  Refresh cadence matches ActivityView (3s).
  const [runningSubagents, setRunningSubagents] = useState([]);
  useEffect(() => {
    if (!activeChatId) { setRunningSubagents([]); return; }
    let cancelled = false;
    const refresh = () => {
      fetch(`/api/activity?chat=${encodeURIComponent(activeChatId)}`)
        .then(r => r.ok ? r.json() : null)
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
  }, [activeChatId]);
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
        {panels.map(ref => {
          const t = tools[ref];
          if (!t) return null;
          return <ToolPanel key={ref} tool={t} onClose={() => onClose(ref)}/>;
        })}
      </div>
    </aside>
  );
}

function ActivityView() {
  // Poll /api/activity so the Subagents lane updates live while a
  // security_engineer run streams.  The registry is authoritative
  // (disk-backed, per chat) — re-fetching is cheap and keeps the
  // tab honest even across tab switches.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const refresh = () => {
      fetch('/api/activity').then(r => r.ok ? r.json() : null).then(act => {
        if (act && Array.isArray(act.lanes)) {
          window.DYSON_DATA.activity = act.lanes;
          setTick(t => t + 1);
        }
      }).catch(() => {});
    };
    refresh();
    const id = setInterval(() => { if (!document.hidden) refresh(); }, 3000);
    return () => clearInterval(id);
  }, []);
  const lanes = (window.DYSON_DATA.activity) || [];
  const running = lanes.filter(a => a.status === 'running').length;
  const grouped = ['subagent','loop','dream','swarm']
    .map(lane => ({ lane, items: lanes.filter(a => a.lane === lane) }))
    .filter(g => g.items.length > 0);
  const fmtDuration = (a) => {
    if (a.status === 'running') return 'running';
    const start = a.started_at || 0;
    const end = a.finished_at || 0;
    if (!start || !end) return '';
    const secs = Math.max(0, end - start);
    if (secs < 60) return `${secs}s`;
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return `${m}m${s.toString().padStart(2,'0')}s`;
  };
  return (
    <div style={{flex:1, overflowY:'auto', padding:'22px 32px', background:'var(--bg-1)'}}>
      <div style={{maxWidth: 980, margin:'0 auto'}}>
        <div className="eyebrow" style={{marginBottom:12}}>
          Background lanes{running > 0 && ` · ${running} running`}
        </div>
        {grouped.length === 0 && (
          <div style={{color:'var(--mute)', fontSize:13, padding:'18px 0'}}>
            No background agents, dreams, or swarm tasks running.
          </div>
        )}
        {grouped.map(({ lane, items }) => {
          const label = lane === 'subagent' ? 'Subagents · orchestrators'
                     : lane === 'loop' ? 'Loops · recurring'
                     : lane === 'dream' ? 'Dreams · background compaction'
                     : 'Swarm · parallel tasks';
          const runningItems = items.filter(a => a.status === 'running');
          const finishedItems = items.filter(a => a.status !== 'running');
          const row = (a, i, dim) => (
            <div key={i} style={{display:'flex', alignItems:'center', gap:14, padding:'10px 14px', background:'var(--bg)', border:'1px solid var(--line)', borderRadius:6, opacity: dim ? 0.72 : 1}}>
              <span style={{width:6, height:6, borderRadius:'50%',
                            background: a.status === 'running' ? 'var(--accent)' : a.status === 'ok' ? 'var(--ok)' : 'var(--err)',
                            animation: a.status === 'running' ? 'pulse 1.4s infinite' : ''}}/>
              <span className="mono" style={{fontSize:12.5, color:'var(--fg)', minWidth:200}}>{a.name}</span>
              <span style={{fontSize:12.5, color:'var(--fg-dim)', flex:1}}>{a.note}</span>
              {a.chat_id && <span className="mono" style={{fontSize:10.5, color:'var(--mute-2)', opacity:0.75}}>{a.chat_id}</span>}
              <span className="mono" style={{fontSize:11, color:'var(--mute-2)'}}>{fmtDuration(a)}</span>
            </div>
          );
          return (
            <div key={lane} style={{marginBottom:22}}>
              <h4 className="eyebrow" style={{margin:'0 0 8px'}}>{label}</h4>
              {runningItems.length > 0 && (
                <div style={{display:'flex', flexDirection:'column', gap:6}}>
                  {runningItems.map((a, i) => row(a, i, false))}
                </div>
              )}
              {finishedItems.length > 0 && (
                <div style={{marginTop: runningItems.length > 0 ? 14 : 0}}>
                  <div className="eyebrow" style={{margin:'0 0 6px', fontSize:10.5, color:'var(--mute-2)'}}>
                    Finished · {finishedItems.length}
                  </div>
                  <div style={{display:'flex', flexDirection:'column', gap:6}}>
                    {finishedItems.map((a, i) => row(a, i, true))}
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ArtefactsView — lists artefacts for the active chat; clicking opens a
// full-screen markdown reader.  Lives at view === 'artefacts'.  The
// per-chat list is hydrated lazily from /api/conversations/:id/artefacts
// once per chat, then kept live by the `onArtefact` SSE callback in App.
// ---------------------------------------------------------------------------

function ArtefactsView({ conv, session, bump }) {
  // Seed the selection from the global stash written by App.onOpenArtefact
  // so deep-linked clicks from chat chips land on the right artefact
  // even though our own event listener isn't attached until after the
  // view switch.  Clear the stash after consuming so it doesn't
  // re-select an old id on the next mount.
  const initialPending = (typeof window !== 'undefined' && window.__dysonOpenArtefactId) || null;
  const [selected, setSelected] = useState(() => {
    if (initialPending) delete window.__dysonOpenArtefactId;
    return initialPending || null;
  });
  // Mobile drawer toggle.  On desktop the sidebar is always visible
  // (grid column), so this state is a no-op there.  On mobile the
  // sidebar is an absolutely-positioned overlay that slides in when
  // `.show-side` is set — defaults to visible (the list) and auto-
  // collapses when the user picks an artefact, so the reader isn't
  // permanently buried under the drawer (the "tap-then-black-screen"
  // bug).  Deep-links boot straight to the reader.
  const [showSide, setShowSide] = useState(!initialPending);

  // Lazy hydrate the per-chat artefact list from the API.  Subsequent
  // switches do nothing — live SSE keeps it current.  We re-run the
  // auto-select below once the fetch resolves so a freshly-loaded
  // chat lands on its newest artefact immediately.
  useEffect(() => {
    if (!conv || !session || !window.DysonLive) return;
    if (session.artefactsLoaded) return;
    session.artefactsLoaded = true;
    window.DysonLive.listArtefacts(conv)
      .then(list => {
        session.artefacts = list || [];
        bump();
      })
      .catch(() => {});
  }, [conv]);

  // When the chat changes from one non-null value to another
  // (= the user clicked a different chat in the sidebar), drop the
  // previous chat's selection so the auto-select effect below picks
  // the newest artefact of the new chat.  A pending id from a chip
  // click or deep-link wins over the auto-pick.  We skip clearing
  // the first time `conv` transitions from null → something (the
  // cold-deep-link case, where the reader has just told App which
  // chat owns the artefact being read — clobbering would throw away
  // the user's actual target).
  const prevConvRef = useRef(conv);
  useEffect(() => {
    const prev = prevConvRef.current;
    prevConvRef.current = conv;
    if (!prev) return;
    const pending = window.__dysonOpenArtefactId;
    if (pending) {
      delete window.__dysonOpenArtefactId;
      setSelected(pending);
      setShowSide(false);
    } else {
      setSelected(null);
      // Switching chats from the LeftRail should land the user back
      // on the artefact list of the new chat, not on whatever reader
      // pane was last open on mobile.
      setShowSide(true);
    }
  }, [conv]);

  // Auto-select the newest artefact for the current chat whenever
  // selection is empty and the list is ready.  Keeps the "click a
  // chat → see a report" flow zero-click and makes the URL reflect
  // the reader position so a back-button or share-link round-trips
  // cleanly.  setShowSide(false) is direct (not via the event round-
  // trip) because the chip-click listener below is registered in a
  // later useEffect — on first mount it isn't attached yet when this
  // effect fires, so dispatching alone would leave the drawer pinned
  // over the reader.
  const listForAutoselect = (session && session.artefacts) || [];
  useEffect(() => {
    if (selected) return;
    if (!listForAutoselect.length) return;
    const first = listForAutoselect[0].id;
    setSelected(first);
    setShowSide(false);
    window.dispatchEvent(new CustomEvent('dyson:open-artefact', { detail: { id: first } }));
  }, [conv, listForAutoselect.length, selected]);

  // Allow in-chat chips to jump straight to a specific artefact.  The
  // event is fired by ArtefactBlock.onClick — if we're already on the
  // Artefacts tab we pick it up; otherwise App's view state change will
  // mount this component and the last-selected id wins.  Closes the
  // mobile drawer so the reader is the visible surface.
  useEffect(() => {
    const h = (e) => {
      const id = e.detail && e.detail.id;
      if (id) { setSelected(id); setShowSide(false); }
    };
    window.addEventListener('dyson:open-artefact', h);
    return () => window.removeEventListener('dyson:open-artefact', h);
  }, []);

  // Deep-link case: `/#/artefacts/<id>` opened cold (no chat selected
  // yet).  Render the reader anyway so the URL genuinely lands the
  // user on the artefact — the reader will learn the owning chat from
  // the fetch response header and restore the sidebar once loaded.
  if (!conv) {
    if (selected) {
      return (
        <div className={`mind${showSide ? ' show-side' : ''}`}>
          {showSide && <div className="mind-scrim" onClick={() => setShowSide(false)}/>}
          <aside className="mind-side">
            <div style={{padding:'10px 14px', borderBottom:'1px solid var(--line)'}}>
              <div className="eyebrow">artefact</div>
              <div style={{fontSize:12, color:'var(--fg-dim)', marginTop:4}}>
                Loading conversation context…
              </div>
            </div>
          </aside>
          <ArtefactReader id={selected} onShowSide={() => setShowSide(true)}/>
        </div>
      );
    }
    return (
      <section className="mind-pane" style={{alignItems:'center', justifyContent:'center'}}>
        <div style={{color:'var(--fg-dim)', fontSize:13}}>Select a conversation to see its artefacts.</div>
      </section>
    );
  }

  const list = (session && session.artefacts) || [];

  return (
    <div className={`mind${showSide ? ' show-side' : ''}`}>
      {/* Tap-to-close target for the mobile drawer.  Mirrors the
          .body.show-left .scrim pattern so the right-strip of visible
          .mind-pane finally has a way to dismiss the 80vw sidebar —
          without this the only escape from an empty drawer was to pick
          an artefact (impossible when the list is empty).  No-op on
          desktop, where the scrim is display:none. */}
      {showSide && <div className="mind-scrim" onClick={() => setShowSide(false)}/>}
      <aside className="mind-side">
        <div style={{padding:'10px 14px', borderBottom:'1px solid var(--line)'}}>
          <div className="eyebrow">artefacts · {list.length}</div>
          <div style={{fontSize:12, color:'var(--fg-dim)', marginTop:4}}>Full-page reports emitted by agents.</div>
        </div>
        {list.length === 0 ? (
          // Centered empty-state so the drawer doesn't read as a broken
          // black page on mobile.  Previously the message was a tiny
          // top-anchored block inside a `flex:1` scroller — below it the
          // rest of the viewport was bare var(--bg), which is what the
          // "still a black screen" report kept pointing at even after
          // the drawer started covering the full viewport.
          <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center', padding:'24px'}}>
            <div style={{color:'var(--fg-dim)', fontSize:13, lineHeight:1.6, textAlign:'center', maxWidth:'320px'}}>
              No artefacts yet in this chat.<br/><br/>
              The security_engineer subagent emits its final report here.<br/><br/>
              Tap <span className="mono" style={{color:'var(--fg)'}}>☰</span> to switch to a chat that already has one.
            </div>
          </div>
        ) : (
          <div style={{overflowY:'auto', flex:1, padding:'6px 0'}}>
            {list.map(a => (
            <div key={a.id}
                 onClick={() => {
                   setSelected(a.id);
                   // Auto-collapse the mobile drawer so the reader
                   // becomes the visible surface — without this the
                   // 80vw sidebar stayed pinned over the reader and
                   // the user just saw a near-black screen.  No-op
                   // on desktop (the sidebar is a grid column there).
                   setShowSide(false);
                   // Surface the id to App so the URL becomes
                   // `#/artefacts/<id>` — clicking through the list
                   // is navigation, not just a local selection.
                   window.dispatchEvent(new CustomEvent('dyson:open-artefact', { detail: { id: a.id } }));
                 }}
                 style={{display:'flex', flexDirection:'column', gap:3, padding:'8px 14px',
                         cursor:'pointer',
                         background: selected === a.id ? 'var(--panel)' : 'transparent',
                         borderLeft: selected === a.id ? '2px solid var(--accent)' : '2px solid transparent'}}>
              <div style={{display:'flex', alignItems:'center', gap:8}}>
                <Icon name="file" size={11} style={{color:'var(--mute)'}}/>
                <span style={{fontSize:12.5, color:'var(--fg)', flex:1, whiteSpace:'nowrap', overflow:'hidden', textOverflow:'ellipsis'}}>{a.title}</span>
                <span className="mono" style={{fontSize:10, color:'var(--mute)'}}>{prettySize(a.bytes || 0)}</span>
              </div>
              <div style={{fontSize:10.5, color:'var(--fg-dim)', paddingLeft:19}}>
                <span className="mono">{(a.kind || 'other').replace(/_/g, ' ')}</span>
                {a.created_at && <span style={{marginLeft:8}}>· {formatAgo(a.created_at)}</span>}
              </div>
            </div>
          ))}
          </div>
        )}
      </aside>
      <ArtefactReader id={selected} onShowSide={() => setShowSide(true)}/>
    </div>
  );
}

// Full-page markdown reader.  Fetches the body from /api/artefacts/:id,
// renders it through the shared `markdown()` helper, and surfaces the
// metadata header (model, target, tokens, cost) in a sticky top bar.
// `onShowSide` (optional) wires up the mobile-only back button so the
// user can re-open the artefact list after picking a report — without
// it the reader would be a one-way door on phones.
function ArtefactReader({ id, onShowSide }) {
  const [body, setBody] = useState('');
  const [meta, setMeta] = useState(null);
  const [err, setErr]  = useState('');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!id || !window.DysonLive) { setBody(''); setMeta(null); setErr(''); return; }
    setErr('');
    const hit = findArtefactMeta(id);
    setMeta(hit);
    // For image artefacts the body returned by /api/artefacts/<id> is
    // just the served URL (pointing at /api/files/<id>).  For markdown
    // artefacts it's the raw content.  Fetching works the same way —
    // the renderer switches on mime_type at display time.
    window.DysonLive.loadArtefact(id)
      .then(({ body, chatId }) => {
        setBody(body);
        // Cold deep-link: tell App which chat owns this artefact so
        // the sidebar restores and the list hydrates.  App listens for
        // this event and calls setConv.
        if (chatId) {
          window.dispatchEvent(new CustomEvent('dyson:set-conv', { detail: { id: chatId } }));
        }
      })
      .catch(e => setErr(String(e.message || e)));
  }, [id]);

  const back = onShowSide
    ? <button className="artefact-back" title="Back to artefact list" onClick={onShowSide}>
        <Icon name="menu" size={14}/>
      </button>
    : null;

  if (!id) {
    // Render the title bar even in the empty state so the mobile back
    // button is reachable — without it the reader is a one-way door
    // when `showSide` is false and `selected` is null (e.g. a chip
    // pointing at a now-deleted artefact, or any state race).  On
    // desktop `.artefact-back` is display:none, leaving just a thin
    // "Artefacts" label bar — harmless.
    return (
      <section className="mind-pane">
        <div style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px',
                     borderBottom:'1px solid var(--line)', background:'var(--bg)'}}>
          {back}
          <span style={{fontSize:13, color:'var(--fg-dim)'}}>Artefacts</span>
        </div>
        <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center',
                     color:'var(--fg-dim)', fontSize:13}}>
          Select an artefact to read.
        </div>
      </section>
    );
  }

  const isImage = meta && typeof meta.kind === 'string' && meta.kind === 'image';
  const imageUrl = isImage
    ? (body && body.startsWith('/') ? body : (meta && meta.metadata && meta.metadata.file_url) || '')
    : '';

  const download = () => {
    if (isImage && imageUrl) {
      const a = document.createElement('a');
      a.href = imageUrl;
      a.download = (meta && meta.metadata && meta.metadata.file_name) || 'image';
      document.body.appendChild(a); a.click(); a.remove();
      return;
    }
    const blob = new Blob([body], { type: 'text/markdown' });
    const u = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = u;
    a.download = ((meta && meta.title) || 'artefact') + '.md';
    document.body.appendChild(a); a.click(); a.remove();
    URL.revokeObjectURL(u);
  };
  // Clipboard API requires a secure context — on HTTP (Tailscale IP,
  // local LAN) `navigator.clipboard` is undefined, so fall back to the
  // hidden-textarea + execCommand dance before giving up.
  const copy = async () => {
    const text = isImage ? imageUrl : body;
    if (!text) return;
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed'; ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
      }
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch (_) { /* clipboard denied — swallow */ }
  };

  return (
    <section className="mind-pane">
      <div style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px',
                   borderBottom:'1px solid var(--line)', background:'var(--bg)', flexWrap:'wrap'}}>
        {back}
        <span style={{fontSize:13, color:'var(--fg)', fontWeight:500}}>{(meta && meta.title) || 'Artefact'}</span>
        {meta && meta.kind && <span className="chip mono">{meta.kind.replace(/_/g, ' ')}</span>}
        {err && <span className="chip" style={{color:'var(--err)'}}>{err}</span>}
        <span style={{flex:1}}/>
        <button className="btn sm ghost" onClick={copy} disabled={isImage ? !imageUrl : !body}>
          {copied ? 'copied' : (isImage ? 'copy url' : 'copy')}
        </button>
        <button className="btn sm primary" onClick={download} disabled={isImage ? !imageUrl : !body}>
          {isImage ? 'download image' : 'download .md'}
        </button>
      </div>
      {meta && meta.metadata && !isImage && (
        <div style={{display:'flex', flexWrap:'wrap', gap:14, padding:'8px 18px',
                     borderBottom:'1px solid var(--line)', background:'var(--panel)', fontSize:11.5}}>
          {metaRow('model',     meta.metadata.model)}
          {metaRow('target',    meta.metadata.target_name)}
          {metaRow('duration',  meta.metadata.duration_seconds, v => `${v}s`)}
          {metaRow('tokens',    meta.metadata.input_tokens, v =>
            `${kfmt(v)} in / ${kfmt(meta.metadata.output_tokens || 0)} out`)}
          {metaRow('cost',      meta.metadata.cost_usd, v => `$${Number(v).toFixed(2)}`)}
          {metaRow('iterations', meta.metadata.iterations)}
        </div>
      )}
      {isImage ? (
        <div style={{overflow:'auto', flex:1, padding:'24px', display:'flex',
                     alignItems:'flex-start', justifyContent:'center', background:'var(--bg)'}}>
          {imageUrl
            ? <img src={imageUrl} alt={(meta && meta.title) || 'image'}
                   style={{maxWidth:'100%', maxHeight:'100%', objectFit:'contain',
                           borderRadius:4, boxShadow:'0 2px 10px rgba(0,0,0,0.1)'}}/>
            : <div style={{color:'var(--mute)', fontSize:13}}>Image no longer available.</div>}
        </div>
      ) : (
        <div className="prose"
             style={{overflowY:'auto', flex:1, padding:'18px 28px', lineHeight:1.6}}
             dangerouslySetInnerHTML={{__html: markdown(body || '')}}/>
      )}
    </section>
  );
}

function metaRow(label, value, fmt) {
  if (value === null || value === undefined || value === '') return null;
  const out = fmt ? fmt(value) : String(value);
  return (
    <div style={{display:'flex', gap:5, alignItems:'baseline'}}>
      <span style={{color:'var(--mute)'}}>{label}</span>
      <span className="mono" style={{color:'var(--fg)'}}>{out}</span>
    </div>
  );
}

function kfmt(n) {
  const v = Number(n) || 0;
  if (v >= 1000) return `${(v / 1000).toFixed(v >= 10000 ? 0 : 1)}k`;
  return String(v);
}

// Walk every session's cached artefact list looking for `id`.  Used by
// ArtefactReader to surface the metadata header without a second fetch.
// Falls through to an empty metadata hit when the id isn't in any
// cached list (e.g. reloaded directly from a URL before the list
// hydrated — rare but non-fatal; the header simply stays blank).
function findArtefactMeta(id) {
  const D = window.DYSON_DATA;
  if (!D) return null;
  // Sessions live in a Map owned by App, not in DYSON_DATA.  Stash a
  // pointer there when App mounts ArtefactsView so this helper can find
  // them without a prop-drill.  See `window.__dysonSessions` below.
  const sessions = window.__dysonSessions;
  if (!sessions) return null;
  for (const s of sessions.values()) {
    const hit = (s.artefacts || []).find(a => a.id === id);
    if (hit) return hit;
  }
  return null;
}

function formatAgo(epochSeconds) {
  const now = Math.floor(Date.now() / 1000);
  const d = Math.max(0, now - epochSeconds);
  if (d < 60) return `${d}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return `${Math.floor(d / 86400)}d ago`;
}

export { TopBar, LeftRail, RightRail, MindView, ActivityView, ArtefactsView, ArtefactReader };
