/* Dyson — secondary views (Mind, Activity, Artefacts).
 *
 * Split out of views.jsx so the initial-paint bundle only contains the
 * Conversation shell (TopBar + LeftRail + RightRail + ConversationView).
 * App.jsx uses React.lazy() against this file — the user pays the ~30 KiB
 * network hop the first time they open one of these tabs, not on cold
 * load.  Keeps LCP under 2.5 s on mobile slow-4G.
 */

import React, { useState, useEffect } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { markdown, prettySize } from './turns.jsx';
import { copyToClipboard } from '../lib/clipboard.js';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useSession } from '../hooks/useSession.js';
import {
  setActivity,
  requestOpenArtefact, clearPendingArtefact,
} from '../store/app.js';
import {
  sessions, updateSession, ensureSession,
} from '../store/sessions.js';

export function MindView({ showSide, onHideSide }) {
  const client = useApi();
  const m = useAppState(s => s.mind);
  const initial = (m.files[0] && m.files[0].path) || '';
  const [selected, setSelected] = useState(initial);
  const [loaded, setLoaded] = useState('');
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState('');

  useEffect(() => {
    if (!selected) { setLoaded(''); setDraft(''); return; }
    client.mindFile(selected)
      .then(file => { const c = file.content || ''; setLoaded(c); setDraft(c); setErr(''); })
      .catch(e => { setErr(String(e.message || e)); setLoaded(''); setDraft(''); });
  }, [selected, client]);

  const dirty = draft !== loaded;
  const save = async () => {
    if (!selected) return;
    setSaving(true); setErr('');
    try {
      await client.postMindFile(selected, draft);
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
          <button className="btn sm primary" onClick={save} disabled={!dirty || saving || !selected}>
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

export function ActivityView() {
  // Poll /api/activity so the Subagents lane updates live while a
  // security_engineer run streams.  The registry is authoritative
  // (disk-backed, per chat) — re-fetching is cheap and keeps the
  // tab honest even across tab switches.
  const client = useApi();
  const lanes = useAppState(s => s.activity);
  useEffect(() => {
    const refresh = () => {
      client.getActivity().then(act => {
        if (act && Array.isArray(act.lanes)) setActivity(act.lanes);
      }).catch(() => {});
    };
    refresh();
    const id = setInterval(() => { if (!document.hidden) refresh(); }, 3000);
    return () => clearInterval(id);
  }, [client]);
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

// Lazy-load a chat's artefact list into the sessions store.  Idempotent —
// if `artefactsLoaded` is already true, we return without refetching.
function ensureArtefacts(chatId, client) {
  if (!chatId) return;
  ensureSession(chatId);
  const existing = sessions.getSnapshot()[chatId];
  if (existing && existing.artefactsLoaded) return;
  updateSession(chatId, s => ({ ...s, artefactsLoaded: true }));
  client.listArtefacts(chatId)
    .then(list => {
      updateSession(chatId, s => ({ ...s, artefacts: list || [] }));
    })
    .catch(() => {
      updateSession(chatId, s => ({ ...s, artefactsLoaded: false }));
    });
}

export function ArtefactsView({ conv, setConv }) {
  const client = useApi();
  const chats = useAppState(s => s.conversations).filter(c => c.hasArtefacts);
  const toggleNonce = useAppState(s => s.ui.toggleArtefactsDrawerNonce);
  const pendingArtefactId = useAppState(s => s.ui.pendingArtefactId);

  // Seed the selection from the store's pendingArtefactId so deep-linked
  // clicks land on the right artefact.  Clear after consuming so it
  // doesn't re-select an old id on the next mount.
  const [selected, setSelected] = useState(() => {
    if (pendingArtefactId) clearPendingArtefact();
    return pendingArtefactId || null;
  });
  // Mobile drawer toggle.  On desktop the sidebar is a permanent grid
  // column, so this state is a no-op there.  On mobile the sidebar is
  // an absolutely-positioned overlay that slides in when `.show-side`
  // is set — defaults to the tree view and collapses to the reader
  // when the user picks an artefact.  Deep-links boot straight to the
  // reader.
  const initialPending = !!pendingArtefactId;
  const [showSide, setShowSide] = useState(!initialPending);
  // Tree expansion state per chat.  Every chat with artefacts is
  // pre-expanded — the Artefacts tab is a flat overview, and hiding
  // sibling-chat reports behind a click made cross-chat browsing a
  // chore.  Users can still collapse a branch they want to mute.
  const [expanded, setExpanded] = useState(() => {
    const init = {};
    for (const c of chats) init[c.id] = true;
    return init;
  });

  // Keep newly-arriving chats expanded too — the chats array fills in
  // asynchronously as conversations load and as artefacts get marked,
  // so any branch that wasn't in the initial snapshot still wants to
  // open by default.  Also fetches artefacts for every expanded branch
  // so the tree paints fully — otherwise pre-expanded branches sit on
  // "No artefacts loaded." until the user clicks them.
  useEffect(() => {
    setExpanded(e => {
      let next = e;
      for (const c of chats) {
        if (!(c.id in next)) {
          if (next === e) next = { ...e };
          next[c.id] = true;
        }
      }
      return next;
    });
    for (const c of chats) ensureArtefacts(c.id, client);
  }, [chats, client]);

  useEffect(() => {
    if (!conv) return;
    ensureArtefacts(conv, client);
  }, [conv, client]);

  // Pick up pendingArtefactId pushed from App or a chat chip — select
  // it, collapse the drawer so the reader is the visible surface, and
  // clear the pending signal.
  useEffect(() => {
    if (!pendingArtefactId) return;
    setSelected(pendingArtefactId);
    setShowSide(false);
    clearPendingArtefact();
  }, [pendingArtefactId]);

  // Hamburger on the Artefacts tab toggles the drawer so users can
  // reopen the tree after picking an artefact — without it the mobile
  // reader is a one-way door until they find the `.artefact-back`
  // button inside the title bar.
  useEffect(() => {
    if (toggleNonce === 0) return;
    setShowSide(s => !s);
  }, [toggleNonce]);

  const activeSession = useSession(conv);
  const activeList = (activeSession && activeSession.artefacts) || [];

  // Auto-select the newest artefact for the current chat whenever
  // selection is empty and the list is ready.
  useEffect(() => {
    if (selected) return;
    if (!conv) return;
    if (!activeList.length) return;
    const first = activeList[0].id;
    setSelected(first);
    setShowSide(false);
  }, [conv, activeList.length, selected]);

  const toggleChat = (chatId) => {
    const willOpen = !expanded[chatId];
    setExpanded(e => ({ ...e, [chatId]: willOpen }));
    if (willOpen) ensureArtefacts(chatId, client);
  };

  const pickArtefact = (chatId, artefactId) => {
    if (chatId && chatId !== conv && setConv) setConv(chatId);
    setSelected(artefactId);
    setShowSide(false);
  };

  // Deep-link: `#/artefacts/<id>` opened cold (no chat known yet).
  // Render the reader anyway — the fetch response header will tell
  // App which chat owns the artefact and setConv will populate the
  // tree on the round-trip.
  const hasChats = chats.length > 0;
  const showDeepLinkPlaceholder = !conv && selected && !hasChats;

  return (
    <div className={`mind${showSide ? ' show-side' : ''}`}>
      {showSide && <div className="mind-scrim" onClick={() => setShowSide(false)}/>}
      <aside className="mind-side">
        <div className="artefact-tree-head">
          <div className="eyebrow">artefacts</div>
          <div style={{fontSize:12, color:'var(--fg-dim)', marginTop:4}}>
            {hasChats
              ? `${chats.length} chat${chats.length === 1 ? '' : 's'} with reports`
              : 'Full-page reports emitted by agents.'}
          </div>
        </div>
        {showDeepLinkPlaceholder ? (
          <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center', padding:'24px'}}>
            <div style={{color:'var(--fg-dim)', fontSize:13, lineHeight:1.6, textAlign:'center', maxWidth:'320px'}}>
              Loading conversation context…
            </div>
          </div>
        ) : hasChats ? (
          <div style={{overflowY:'auto', flex:1, padding:'4px 0'}}>
            {chats.map(c => {
              const isActive = c.id === conv;
              const open = !!expanded[c.id];
              return <ChatBranch key={c.id} chat={c} isActive={isActive} open={open}
                                  onToggle={() => toggleChat(c.id)}
                                  selected={selected}
                                  onPick={(id) => pickArtefact(c.id, id)}/>;
            })}
          </div>
        ) : (
          <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center', padding:'24px'}}>
            <div style={{color:'var(--fg-dim)', fontSize:13, lineHeight:1.6, textAlign:'center', maxWidth:'320px'}}>
              No artefacts yet.<br/><br/>
              The security_engineer subagent emits its final report here.
            </div>
          </div>
        )}
      </aside>
      <ArtefactReader id={selected} onShowSide={() => setShowSide(true)} client={client}/>
    </div>
  );
}

// One chat row in the tree.  Subscribes to the chat's own session so
// its artefact list re-renders when the fetch lands, without forcing
// the parent ArtefactsView to re-render on every sibling update.
function ChatBranch({ chat, isActive, open, onToggle, selected, onPick }) {
  const s = useSession(chat.id);
  const items = (s && s.artefacts) || [];
  return (
    <div>
      <div className="artefact-chat-row"
           data-active={isActive ? 'true' : 'false'}
           onClick={onToggle}>
        <span className="caret" style={{transform: open ? 'rotate(90deg)' : 'none'}}>
          <Icon name="chev" size={10}/>
        </span>
        <span className="title">{chat.title || '(untitled)'}</span>
        {items.length > 0 && <span className="count mono">{items.length}</span>}
      </div>
      {open && (
        items.length === 0 ? (
          <div style={{padding:'6px 14px 10px 32px', color:'var(--fg-dim)', fontSize:11.5}}>
            No artefacts loaded.
          </div>
        ) : (
          items.map(a => (
            <div key={a.id}
                 className="artefact-row"
                 data-selected={selected === a.id ? 'true' : 'false'}
                 onClick={() => onPick(a.id)}>
              <Icon name="file" size={11} style={{color:'var(--mute)'}}/>
              <span className="title">{a.title}</span>
              <span className="mono size">{prettySize(a.bytes || 0)}</span>
            </div>
          ))
        )
      )}
    </div>
  );
}

// Walk every session's cached artefact list looking for `id`.  Used by
// ArtefactReader to surface metadata without a second fetch.  Falls
// through to null when the id isn't in any cached list (rare but
// non-fatal; the header simply stays blank).
function findArtefactMeta(id) {
  const snap = sessions.getSnapshot();
  for (const chatId of Object.keys(snap)) {
    const arts = snap[chatId].artefacts || [];
    const hit = arts.find(a => a.id === id);
    if (hit) return hit;
  }
  return null;
}

// Full-page markdown reader.  Fetches the body from /api/artefacts/:id,
// renders it through the shared `markdown()` helper, and surfaces the
// metadata header (model, target, tokens, cost) in a sticky top bar.
// `onShowSide` (optional) wires up the mobile-only back button so the
// user can re-open the artefact list after picking a report.  `client`
// (optional) overrides the React context — ArtefactsView passes its
// own client in so the reader doesn't need a second useApi() lookup.
export function ArtefactReader({ id, onShowSide, client: clientProp }) {
  const ctxClient = useApi();
  const client = clientProp || ctxClient;
  const [body, setBody] = useState('');
  const [meta, setMeta] = useState(null);
  const [err, setErr]  = useState('');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!id || !client) { setBody(''); setMeta(null); setErr(''); return; }
    setErr('');
    const hit = findArtefactMeta(id);
    setMeta(hit);
    client.loadArtefact(id)
      .then(({ body, chatId }) => {
        setBody(body);
        if (chatId) requestOpenArtefact(id);
        // Side-effect: upsert the owning chat row if the store doesn't
        // know about it yet (cold deep-link).  The conversations list
        // polling will flesh out the title on the next tick.
        if (chatId) {
          // We don't know the title; leave the existing row alone and
          // only insert a skeleton if missing.  App's URL effect will
          // take it from here.
        }
      })
      .catch(e => setErr(String(e.message || e)));
  }, [id, client]);

  const back = onShowSide
    ? <button className="artefact-back" title="Back to artefact list" onClick={onShowSide}>
        <Icon name="menu" size={14}/>
      </button>
    : null;

  if (!id) {
    // Render the title bar even in the empty state so the mobile back
    // button is reachable — without it the reader is a one-way door
    // when `showSide` is false and `selected` is null.
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
  const copy = async () => {
    const text = isImage ? imageUrl : body;
    if (await copyToClipboard(text)) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
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
