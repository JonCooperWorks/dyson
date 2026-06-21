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
import { ShareMenu } from './share-menu.jsx';
import { copyToClipboard } from '../lib/clipboard.js';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useSession } from '../hooks/useSession.js';
import { useEscapeKey } from '../hooks/useEscapeKey.js';
import {
  setActivity,
  requestOpenArtefact, clearPendingArtefact,
} from '../store/app.js';
import {
  sessions, updateSession, ensureSession,
} from '../store/sessions.js';

export function MindView({ showSide, onHideSide, path, setPath }) {
  const client = useApi();
  // Esc closes the mobile workspace drawer when it's open.
  useEscapeKey(showSide ? onHideSide : null);
  const m = useAppState(s => s.mind);
  // Selection is owned by the URL hash so the back button moves
  // between selected files and a deep-link / refresh restores the
  // last-open file.  When no path is supplied (cold load with bare
  // `#/mind`), fall through to the first workspace entry once the
  // file list arrives — selecting it propagates back to the URL via
  // `setPath`.
  const selected = path || '';
  const setSelected = (p) => { if (typeof setPath === 'function') setPath(p || null); };
  const [loaded, setLoaded] = useState('');
  const [draft, setDraft] = useState('');
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState('');

  useEffect(() => {
    // Cold open with no `#/mind/<path>`: pick the first file the
    // workspace knows about so the editor isn't blank.  Subsequent
    // picks are user-driven via setSelected.
    if (selected) return;
    if (m.files.length === 0) return;
    setSelected(m.files[0].path);
  }, [selected, m.files]);

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
      {showSide && <div className="mind-scrim" onClick={onHideSide}/>}
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
  const grouped = ['subagent','loop','dream']
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
            No background agents or dreams running.
          </div>
        )}
        {grouped.map(({ lane, items }) => {
          const label = lane === 'subagent' ? 'Subagents · orchestrators'
                     : lane === 'loop' ? 'Loops · recurring'
                     : 'Dreams · background compaction';
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
// AuditView — per-Dyson LLM request audit.  Lists every proxied call
// (swarm is the source of truth) with cost, tokens, latency, and output
// throughput (tok/s).  Lives at view === 'audit'.  Polls /api/audit,
// which forwards to swarm's per-instance internal endpoint.
// ---------------------------------------------------------------------------

const AUDIT_RANGES = [
  { id: 'today', label: 'Today' },
  { id: '7d',    label: '7d' },
  { id: '30d',   label: '30d' },
  { id: 'all',   label: 'All' },
];

function fmtUsd(v) {
  if (v == null || !Number.isFinite(v)) return '—';
  if (v === 0) return '$0';
  if (v < 0.01) return `$${v.toFixed(v < 0.0001 ? 6 : 4).replace(/0+$/u, '').replace(/\.$/u, '')}`;
  return `$${v.toFixed(2)}`;
}
function fmtNum(v) {
  const n = Number(v || 0);
  if (n >= 1e6) return `${(n / 1e6).toFixed(1).replace(/\.0$/u, '')}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1).replace(/\.0$/u, '')}k`;
  return String(Math.max(0, Math.round(n)));
}
function fmtToks(v) {
  if (v == null || !Number.isFinite(v)) return '—';
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} tok/s`;
}
function fmtMs(v) {
  if (v == null) return '—';
  if (v < 1000) return `${v}ms`;
  return `${(v / 1000).toFixed(v < 10000 ? 2 : 1)}s`;
}
function fmtAgo(secs) {
  if (!secs) return '';
  const d = Math.max(0, Math.floor(Date.now() / 1000) - secs);
  if (d < 60) return `${d}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return `${Math.floor(d / 86400)}d ago`;
}

function AuditRow({ r }) {
  const [open, setOpen] = useState(false);
  const live = r.cost_source === 'missing' && (r.output_tokens == null);
  const statusOk = r.status_code >= 200 && r.status_code < 300;
  const reconciled = r.reconciled_at != null;
  const costBadge = reconciled ? 'reconciled'
                  : r.cost_source === 'provider_reported' ? 'provider'
                  : r.cost_source === 'pricing_table' ? 'estimated'
                  : 'unpriced';
  const badgeColor = costBadge === 'reconciled' ? 'var(--accent)'
                   : costBadge === 'provider' ? 'var(--ok)'
                   : costBadge === 'estimated' ? 'var(--warn, #c08a2a)'
                   : 'var(--mute-2)';
  return (
    <div style={{border:'1px solid var(--line)', borderRadius:6, background:'var(--bg)', overflow:'hidden'}}>
      <div onClick={() => setOpen(o => !o)}
           style={{display:'flex', alignItems:'center', gap:12, padding:'9px 12px', cursor:'pointer'}}>
        <span style={{width:6, height:6, borderRadius:'50%', flexShrink:0,
                      background: statusOk ? 'var(--ok)' : 'var(--err)',
                      animation: live ? 'pulse 1.4s infinite' : ''}}/>
        <span className="mono" style={{fontSize:12, color:'var(--fg)', minWidth:170, overflow:'hidden', textOverflow:'ellipsis', whiteSpace:'nowrap'}}>
          {r.model || r.provider}
        </span>
        <span style={{fontSize:11, color:'var(--mute-2)', minWidth:78}}>{r.provider}</span>
        <span className="mono" style={{fontSize:11.5, color:'var(--fg-dim)', minWidth:96}}>
          {fmtNum(r.input_tokens)}→{fmtNum(r.output_tokens)}
        </span>
        <span className="mono" style={{fontSize:11.5, color: r.tok_per_sec != null ? 'var(--fg)' : 'var(--mute-2)', minWidth:78}}>
          {fmtToks(r.tok_per_sec)}
        </span>
        <span className="mono" style={{fontSize:11, color:'var(--mute-2)', minWidth:54}} title="time to first token">
          {fmtMs(r.ttft_ms)}
        </span>
        <span style={{flex:1}}/>
        <span style={{fontSize:10, padding:'1px 6px', borderRadius:4, color:badgeColor, border:`1px solid ${badgeColor}`, opacity:0.9}}>
          {costBadge}
        </span>
        <span className="mono" style={{fontSize:12, color:'var(--fg)', minWidth:62, textAlign:'right'}}>
          {fmtUsd(r.cost_usd)}
        </span>
        {!statusOk && <span className="mono" style={{fontSize:10.5, color:'var(--err)'}}>{r.status_code}</span>}
        <span className="mono" style={{fontSize:10.5, color:'var(--mute-2)', minWidth:62, textAlign:'right'}}>{fmtAgo(r.occurred_at)}</span>
      </div>
      {open && (
        <div style={{padding:'4px 12px 11px 24px', display:'grid', gridTemplateColumns:'auto 1fr', columnGap:14, rowGap:3, fontSize:11.5, color:'var(--fg-dim)', borderTop:'1px solid var(--line)'}}>
          <span style={{color:'var(--mute-2)'}}>key source</span><span className="mono">{r.key_source}</span>
          <span style={{color:'var(--mute-2)'}}>status</span><span className="mono">{r.status_code}</span>
          <span style={{color:'var(--mute-2)'}}>tokens</span><span className="mono">in {fmtNum(r.input_tokens)} · out {fmtNum(r.output_tokens)}{r.native_output_tokens != null ? ` · native ${fmtNum(r.native_output_tokens)}` : ''}</span>
          <span style={{color:'var(--mute-2)'}}>latency</span><span className="mono">ttft {fmtMs(r.ttft_ms)} · stream {fmtMs(r.stream_ms)}{r.gen_time_ms != null ? ` · gen ${fmtMs(r.gen_time_ms)}` : ''}</span>
          <span style={{color:'var(--mute-2)'}}>throughput</span><span className="mono">{fmtToks(r.tok_per_sec)}{reconciled ? ' (reconciled)' : r.stream_ms != null ? ' (local)' : ''}</span>
          <span style={{color:'var(--mute-2)'}}>cost source</span><span className="mono">{r.cost_source}{reconciled ? ` · reconciled ${fmtAgo(r.reconciled_at)}` : ''}</span>
          {r.upstream_generation_id && (<><span style={{color:'var(--mute-2)'}}>generation</span><span className="mono" style={{wordBreak:'break-all'}}>{r.upstream_generation_id}</span></>)}
          <span style={{color:'var(--mute-2)'}}>audit id</span><span className="mono">{r.audit_id}</span>
        </div>
      )}
    </div>
  );
}

export function AuditView() {
  const client = useApi();
  const [range, setRange] = useState('7d');
  const [rows, setRows] = useState([]);
  const [source, setSource] = useState('swarm');
  const [loaded, setLoaded] = useState(false);
  useEffect(() => {
    let alive = true;
    const refresh = () => {
      client.getAudit({ range, limit: 300 }).then(res => {
        if (!alive) return;
        setRows(Array.isArray(res?.requests) ? res.requests : []);
        setSource(res?.source || 'swarm');
        setLoaded(true);
      }).catch(() => { if (alive) setLoaded(true); });
    };
    refresh();
    const id = setInterval(() => { if (!document.hidden) refresh(); }, 5000);
    return () => { alive = false; clearInterval(id); };
  }, [client, range]);

  const totals = rows.reduce((acc, r) => {
    acc.cost += Number(r.cost_usd || 0);
    acc.tokens += Number(r.total_tokens || (Number(r.input_tokens || 0) + Number(r.output_tokens || 0)));
    if (r.tok_per_sec != null) { acc.tpsSum += r.tok_per_sec; acc.tpsN += 1; }
    if (!(r.status_code >= 200 && r.status_code < 300)) acc.errors += 1;
    return acc;
  }, { cost: 0, tokens: 0, tpsSum: 0, tpsN: 0, errors: 0 });
  const avgTps = totals.tpsN > 0 ? totals.tpsSum / totals.tpsN : null;

  const stat = (label, value, color) => (
    <div style={{display:'flex', flexDirection:'column', gap:2}}>
      <span className="eyebrow" style={{fontSize:10, color:'var(--mute-2)'}}>{label}</span>
      <span className="mono" style={{fontSize:15, color: color || 'var(--fg)'}}>{value}</span>
    </div>
  );

  return (
    <div style={{flex:1, overflowY:'auto', padding:'22px 32px', background:'var(--bg-1)'}}>
      <div style={{maxWidth: 1080, margin:'0 auto'}}>
        <div style={{display:'flex', alignItems:'center', justifyContent:'space-between', marginBottom:14, flexWrap:'wrap', gap:10}}>
          <div className="eyebrow">LLM Audit · {rows.length} request{rows.length === 1 ? '' : 's'}</div>
          <div style={{display:'flex', gap:4}}>
            {AUDIT_RANGES.map(r => (
              <button key={r.id} onClick={() => setRange(r.id)}
                      style={{fontSize:11.5, padding:'3px 10px', borderRadius:5, cursor:'pointer',
                              border:'1px solid var(--line)',
                              background: range === r.id ? 'var(--accent)' : 'transparent',
                              color: range === r.id ? '#fff' : 'var(--fg-dim)'}}>
                {r.label}
              </button>
            ))}
          </div>
        </div>

        <div style={{display:'flex', gap:30, padding:'14px 16px', marginBottom:16, border:'1px solid var(--line)', borderRadius:8, background:'var(--bg)', flexWrap:'wrap'}}>
          {stat('Requests', String(rows.length))}
          {stat('Spend', fmtUsd(totals.cost))}
          {stat('Tokens', fmtNum(totals.tokens))}
          {stat('Avg throughput', avgTps != null ? fmtToks(avgTps) : '—')}
          {stat('Errors', String(totals.errors), totals.errors > 0 ? 'var(--err)' : 'var(--fg)')}
        </div>

        {source !== 'swarm' && (
          <div style={{color:'var(--mute)', fontSize:12.5, padding:'10px 0 16px'}}>
            {source === 'unavailable'
              ? 'Per-request audit is served by Swarm. This Dyson is running standalone, so no rows are available.'
              : 'Could not reach Swarm for audit data. Showing nothing rather than stale numbers.'}
          </div>
        )}

        {loaded && rows.length === 0 && source === 'swarm' && (
          <div style={{color:'var(--mute)', fontSize:13, padding:'18px 0'}}>No LLM requests in this range yet.</div>
        )}

        <div style={{display:'flex', flexDirection:'column', gap:5}}>
          {rows.map(r => <AuditRow key={r.audit_id} r={r}/>)}
        </div>
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

function initialArtefactChatId(chats, conv) {
  if (conv && chats.some(c => c.id === conv)) return conv;
  return chats[0]?.id || null;
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
  const [selectedChatId, setSelectedChatId] = useState(() => (pendingArtefactId && conv ? conv : null));
  // Mobile drawer toggle.  On desktop the sidebar is a permanent grid
  // column, so this state is a no-op there.  On mobile the sidebar is
  // an absolutely-positioned overlay that slides in when `.show-side`
  // is set — defaults to the tree view and collapses to the reader
  // when the user picks an artefact.  Deep-links boot straight to the
  // reader.
  const initialPending = !!pendingArtefactId;
  const [showSide, setShowSide] = useState(!initialPending);
  // Esc closes the mobile artefacts drawer when it's open.
  useEscapeKey(showSide ? () => setShowSide(false) : null);
  // Tree expansion state per chat.  Keep the current chat (or first
  // report-bearing chat) open, then hydrate sibling branches on demand.
  // A 30-chat workspace used to fan out 30 artefact requests on first
  // mobile paint, which made the drawer feel broken and could starve
  // the live proxy behind it.
  const firstOpenChatId = initialArtefactChatId(chats, conv);
  const [expanded, setExpanded] = useState(() => {
    const init = {};
    if (firstOpenChatId) init[firstOpenChatId] = true;
    return init;
  });

  // Keep one useful branch open as conversations arrive asynchronously,
  // but do not auto-expand every report-bearing chat.  Sibling branches
  // fetch lazily from `toggleChat`.
  useEffect(() => {
    const target = initialArtefactChatId(chats, conv);
    if (!target) return;
    setExpanded(e => {
      if (e[target]) return e;
      return { ...e, [target]: true };
    });
    ensureArtefacts(target, client);
  }, [chats, conv, client]);

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
    setSelectedChatId(conv || null);
    setShowSide(false);
    clearPendingArtefact();
  }, [pendingArtefactId, conv]);

  // Hamburger on the Artefacts tab toggles the drawer so users can
  // reopen the tree after picking an artefact. Keeping drawer access
  // in the topbar avoids duplicate menu buttons inside the reader.
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
    setSelectedChatId(conv);
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
    setSelectedChatId(chatId || null);
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
                                  selectedChatId={selectedChatId}
                                  onPick={(id) => pickArtefact(c.id, id)}/>;
            })}
          </div>
        ) : (
          <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center', padding:'24px'}}>
            <div style={{color:'var(--fg-dim)', fontSize:13, lineHeight:1.6, textAlign:'center', maxWidth:'320px'}}>
              No artefacts yet.
            </div>
          </div>
        )}
      </aside>
      <ArtefactReader id={selected} chatId={selectedChatId} client={client}/>
    </div>
  );
}

// One chat row in the tree.  Subscribes to the chat's own session so
// its artefact list re-renders when the fetch lands, without forcing
// the parent ArtefactsView to re-render on every sibling update.
function ChatBranch({ chat, isActive, open, onToggle, selected, selectedChatId, onPick }) {
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
                 data-selected={selected === a.id && selectedChatId === chat.id ? 'true' : 'false'}
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
function findArtefactMeta(id, chatId = null) {
  const snap = sessions.getSnapshot();
  if (chatId && snap[chatId]) {
    const arts = snap[chatId].artefacts || [];
    const hit = arts.find(a => a.id === id);
    if (hit) return { ...hit, chat_id: chatId };
  }
  for (const candidateChatId of Object.keys(snap)) {
    const arts = snap[candidateChatId].artefacts || [];
    const hit = arts.find(a => a.id === id);
    if (hit) return { ...hit, chat_id: hit.chat_id || candidateChatId };
  }
  return null;
}

// Full-page markdown reader.  Fetches the body from /api/artefacts/:id,
// renders it through the shared `markdown()` helper, and surfaces the
// metadata header (model, target, tokens, cost) in a sticky top bar.
// `client` (optional) overrides the React context — ArtefactsView
// passes its own client in so the reader doesn't need a second useApi()
// lookup.
export function ArtefactReader({ id, chatId: requestedChatId = null, client: clientProp }) {
  const ctxClient = useApi();
  const client = clientProp || ctxClient;
  const [body, setBody] = useState('');
  const [filePreview, setFilePreview] = useState('');
  const [filePreviewErr, setFilePreviewErr] = useState('');
  const [meta, setMeta] = useState(null);
  const [err, setErr]  = useState('');
  const [copied, setCopied] = useState(false);
  const [chatId, setChatId] = useState(null);
  // Share affordance state.  All useState/useEffect for the share
  // flow MUST live above the `if (!id)` early-return so React's
  // hook count stays stable across the empty-state and loaded-state
  // renders — otherwise we'd hit "Rendered more hooks than during
  // the previous render" the moment the user navigates away from a
  // selected artefact.
  const [shareBusy, setShareBusy] = useState(false);
  const [shareUrl, setShareUrl] = useState(null);
  const [shareErr, setShareErr] = useState('');
  const [shareCopied, setShareCopied] = useState(false);

  useEffect(() => {
    if (!id || !client) { setBody(''); setMeta(null); setErr(''); setChatId(null); return; }
    setErr('');
    setBody('');
    setFilePreview('');
    setFilePreviewErr('');
    setShareUrl(null);
    setShareErr('');
    const hit = findArtefactMeta(id, requestedChatId);
    setMeta(hit);
    const scopedChatId = requestedChatId || (hit && hit.chat_id) || null;
    setChatId(scopedChatId);
    client.loadArtefact(id, scopedChatId)
      .then(({ body, chatId: cid }) => {
        setBody(body);
        if (cid) {
          setChatId(cid);
          if (!requestedChatId) requestOpenArtefact(id);
        }
      })
      .catch(e => setErr(String(e.message || e)));
  }, [id, requestedChatId, client]);

  const metaData = (meta && meta.metadata) || {};
  const metaFileUrl = metaData.file_url || '';
  const metaFileMime = metaData.mime_type || '';
  const metaFileName = metaData.file_name || (meta && meta.title) || '';
  const metaFileBytes = typeof metaData.bytes === 'number' ? metaData.bytes : null;
  const isTextLikeFile = Boolean(metaFileUrl) && previewableTextFile(metaFileMime, metaFileName);
  const shouldFetchFilePreview = isTextLikeFile && bodyLooksLikeFileUrl(body, metaFileUrl);

  useEffect(() => {
    if (!id || !isTextLikeFile || !shouldFetchFilePreview || !metaFileUrl || !client) {
      setFilePreview('');
      setFilePreviewErr('');
      return;
    }
    let cancelled = false;
    setFilePreview('');
    setFilePreviewErr('');
    const loader = typeof client.loadFileText === 'function'
      ? client.loadFileText(metaFileUrl)
      : Promise.reject(new Error('text preview unavailable'));
    loader
      .then(text => { if (!cancelled) setFilePreview(text || ''); })
      .catch(e => { if (!cancelled) setFilePreviewErr(String(e.message || e)); });
    return () => { cancelled = true; };
  }, [id, isTextLikeFile, shouldFetchFilePreview, metaFileUrl, client]);

  if (!id) {
    // Render the title bar even in the empty state so the reader
    // preserves the same chrome as loaded artefacts. The topbar
    // hamburger owns drawer access on mobile.
    return (
      <section className="mind-pane">
        <div className="artefact-reader-head"
             style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px',
                     borderBottom:'1px solid var(--line)', background:'var(--bg)'}}>
          <span className="artefact-reader-title" style={{fontSize:13, color:'var(--fg-dim)'}}>Artefacts</span>
        </div>
        <div style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center',
                     color:'var(--fg-dim)', fontSize:13}}>
          Select an artefact to read.
        </div>
      </section>
    );
  }

  const isImage = meta && typeof meta.kind === 'string' && meta.kind === 'image';
  // Image artefacts: the agent stamps `metadata.file_url` at emit
  // time (see output.rs / state.rs send_file paths) AND mirrors the
  // same URL into the raw artefact body, so cold deep-links that
  // arrive before `meta` hydrates can still paint by reading `body`.
  // Without this resolution the `<img>` branch below was unreachable
  // for every image and the reader misleadingly said "Image no
  // longer available."
  const imageUrl = isImage
    ? ((meta && meta.metadata && meta.metadata.file_url) || body || '')
    : '';
  // A `kind:'other'` artefact paired with metadata.file_url is a sent
  // file (anything send_file emitted that wasn't an image).  For
  // markdown the body field IS the file text (set server-side), so the
  // existing markdown render path below handles it without any extra
  // fetch.  Non-markdown files fall through to a download card.
  const fileUrl = !isImage && meta && meta.metadata && meta.metadata.file_url
    ? meta.metadata.file_url
    : '';
  const fileMime = metaFileMime;
  const fileName = metaFileName;
  const fileBytes = metaFileBytes;
  const isFile = Boolean(fileUrl);
  const isMarkdownFile = isFile && previewableMarkdownFile(fileMime, fileName);
  const isPreviewableFile = isFile && previewableTextFile(fileMime, fileName);
  const previewBody = isPreviewableFile
    ? (bodyLooksLikeFileUrl(body, fileUrl) ? filePreview : body)
    : '';
  const previewLoading = isPreviewableFile && bodyLooksLikeFileUrl(body, fileUrl) && !filePreview && !filePreviewErr;
  // Markdown/text files use an inline reader. Other files (binary,
  // archives, images handled above) get a download-only card.
  const isBinaryFile = isFile && !isPreviewableFile;

  const download = () => {
    const url = isImage ? imageUrl : fileUrl;
    if ((isImage || isFile) && url) {
      const a = document.createElement('a');
      a.href = url;
      a.download = fileName || (isImage ? 'image' : 'file');
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
    const text = isImage ? imageUrl : (isBinaryFile ? fileUrl : isPreviewableFile ? previewBody : body);
    if (await copyToClipboard(text)) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  };

  const downloadLabel = isImage ? 'download image'
    : isFile ? 'download'
    : 'download .md';

  // Anonymous share — mint same-origin via the swarm escape route on
  // dyson_proxy.  `<id>.<apex>/_swarm/share-mint` is intercepted
  // before the request reaches the cube; swarm uses the user identity
  // already resolved on the way in (cookie or Authorization) and
  // calls ShareService::mint server-side.  No cross-origin fetch
  // needed; the URL lands in `shareUrl` for one-click copy.
  const canShare = Boolean(id && chatId);
  const mintShare = async (ttl) => {
    if (!canShare) return;
    setShareBusy(true);
    setShareErr('');
    try {
      const r = await fetch('/_swarm/share-mint', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          artefact_id: id,
          chat_id: chatId,
          ttl,
        }),
      });
      if (!r.ok) {
        const text = await r.text().catch(() => '');
        throw new Error(`HTTP ${r.status}: ${text || 'mint failed'}`);
      }
      const m = await r.json();
      setShareUrl(m.url || null);
    } catch (e) {
      setShareErr(String(e.message || e));
    } finally {
      setShareBusy(false);
    }
  };
  const copyShareUrl = async () => {
    if (!shareUrl) return;
    if (await copyToClipboard(shareUrl)) {
      setShareCopied(true);
      setTimeout(() => setShareCopied(false), 1500);
    }
  };

  return (
    <section className="mind-pane">
      <div className="artefact-reader-head"
           style={{display:'flex', alignItems:'center', gap:10, padding:'10px 18px',
                   borderBottom:'1px solid var(--line)', background:'var(--bg)', flexWrap:'wrap'}}>
        <span className="artefact-reader-title" style={{fontSize:13, color:'var(--fg)', fontWeight:500}}>{(meta && meta.title) || 'Artefact'}</span>
        {meta && meta.kind && <span className="chip mono">{meta.kind.replace(/_/g, ' ')}</span>}
        {err && <span className="chip" style={{color:'var(--err)'}}>{err}</span>}
        <span className="artefact-reader-spacer" style={{flex:1}}/>
        <ShareMenu
          canShare={canShare}
          busy={shareBusy}
          onMint={mintShare}
        />
        <button className="btn sm ghost" onClick={copy} disabled={isImage ? !imageUrl : isBinaryFile ? !fileUrl : isPreviewableFile ? !previewBody : !body}>
          <Icon name="copy" size={12}/>
          <span className="btn-label">{copied ? 'copied' : (isImage || isBinaryFile ? 'copy url' : 'copy')}</span>
        </button>
        <button className="btn sm primary" onClick={download} disabled={isImage ? !imageUrl : isFile ? !fileUrl : !body}>
          <Icon name="download" size={12}/>
          <span className="btn-label">{downloadLabel}</span>
        </button>
      </div>
      {(shareUrl || shareErr) && (
        <div style={{
          padding: '10px 18px', borderBottom: '1px solid var(--line)',
          background: 'var(--panel)', display: 'flex', flexWrap: 'wrap',
          alignItems: 'center', gap: 10, fontSize: 12,
        }}>
          {shareErr ? (
            <>
              <span style={{ color: 'var(--err)' }}>share failed: {shareErr}</span>
              <button className="btn xs ghost" onClick={() => setShareErr('')}>dismiss</button>
            </>
          ) : (
            <>
              <span style={{ color: 'var(--mute)' }}>anonymous share URL:</span>
              <code className="mono" style={{
                flex: 1, minWidth: 0,
                overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                color: 'var(--fg)',
              }} title={shareUrl}>{shareUrl}</code>
              <button className="btn xs primary" onClick={copyShareUrl}>
                {shareCopied ? 'copied' : 'copy'}
              </button>
              <button className="btn xs ghost" onClick={() => setShareUrl(null)}>dismiss</button>
            </>
          )}
        </div>
      )}
      {meta && meta.metadata && !isImage && !isFile && (
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
      {isFile && (
        <div style={{display:'flex', flexWrap:'wrap', gap:14, padding:'8px 18px',
                     borderBottom:'1px solid var(--line)', background:'var(--panel)', fontSize:11.5}}>
          {metaRow('name', fileName)}
          {metaRow('mime', fileMime || 'application/octet-stream')}
          {fileBytes !== null && metaRow('size', fileBytes, prettySize)}
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
      ) : isBinaryFile ? (
        <div className="artefact-file-card"
             style={{flex:1, display:'flex', alignItems:'center', justifyContent:'center',
                     padding:'40px 24px', textAlign:'center', color:'var(--fg-dim)', fontSize:13}}>
          <div>
            <div style={{fontSize:14, color:'var(--fg)', marginBottom:6}}>{fileName}</div>
            <div style={{marginBottom:18, fontSize:12}}>
              {fileMime || 'binary file'}
              {fileBytes !== null ? ` · ${prettySize(fileBytes)}` : ''}
            </div>
            <button className="btn primary" onClick={download} disabled={!fileUrl}>
              Download
            </button>
          </div>
        </div>
      ) : isMarkdownFile ? (
        <div className="prose"
             style={{overflowY:'auto', flex:1, padding:'18px 28px', lineHeight:1.6}}
             dangerouslySetInnerHTML={{__html: markdown(previewBody || (previewLoading ? 'Loading preview…' : ''))}}/>
      ) : isPreviewableFile ? (
        <div className="artefact-text-reader">
          {filePreviewErr ? (
            <div className="artefact-preview-error">
              Preview failed: {filePreviewErr}
            </div>
          ) : previewLoading ? (
            <div className="artefact-preview-empty">Loading preview…</div>
          ) : (
            <pre className="artefact-text-preview">{previewBody}</pre>
          )}
        </div>
      ) : (
        <div className="prose"
             style={{overflowY:'auto', flex:1, padding:'18px 28px', lineHeight:1.6}}
             dangerouslySetInnerHTML={{__html: markdown(body || '')}}/>
      )}
    </section>
  );
}

function normalMime(mime) {
  return String(mime || '').split(';', 1)[0].trim().toLowerCase();
}

function previewableMarkdownFile(mime, name) {
  const m = normalMime(mime);
  return m === 'text/markdown' || m === 'text/x-markdown' || /\.(md|markdown)$/i.test(name || '');
}

function previewableTextFile(mime, name) {
  const m = normalMime(mime);
  if (previewableMarkdownFile(mime, name)) return true;
  if (m.startsWith('text/')) return true;
  if ([
    'application/json',
    'application/ld+json',
    'application/xml',
    'application/xhtml+xml',
    'application/javascript',
    'application/x-javascript',
    'application/x-sh',
    'application/x-yaml',
    'application/toml',
  ].includes(m)) return true;
  return /\.(txt|log|json|jsonl|csv|tsv|ya?ml|toml|ini|env|css|html?|xml|js|jsx|ts|tsx|mjs|cjs|py|rb|go|rs|java|c|cc|cpp|h|hpp|sh|bash|zsh|sql)$/i.test(name || '');
}

function bodyLooksLikeFileUrl(body, fileUrl) {
  const text = String(body || '').trim();
  return Boolean(text && fileUrl && text === String(fileUrl).trim());
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
