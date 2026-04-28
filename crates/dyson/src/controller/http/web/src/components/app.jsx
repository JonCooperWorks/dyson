/* Dyson — app root.  Live mode only.
 *
 * Per-chat state lives in the sessions store (../store/sessions.js).
 * Switching `conv` swaps which session slice the view reads but never
 * touches any other chat's state.  The old version held sessions as a
 * ref-to-Map and mutated fields in place, re-rendering via a `bump()`
 * counter — React's reconciler saw the same Map reference across
 * re-renders and silently dropped subtree updates.  Every SSE delta
 * now produces a new frozen snapshot; no mutation, no bump, no muddle.
 */

import React, { useState, useEffect, useRef, useCallback, useLayoutEffect, Suspense, lazy } from 'react';
import { Icon } from './icons.jsx';
import { Turn, Composer, TypingIndicator, EmptyState } from './turns.jsx';
import { TopBar, LeftRail, RightRail } from './views.jsx';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useSession, useSessionMutator } from '../hooks/useSession.js';
import {
  setTool, updateTool, mergeTools, upsertConversation,
  markConversationHasArtefacts,
  requestOpenRail, requestOpenArtefact, clearPendingArtefact,
  requestToggleArtefactsDrawer,
} from '../store/app.js';
import {
  ensureSession, updateSession, getSession, getResources, mintToolRef,
  mapLastTurn, appendBlock, mapAgentTail, appendAgentBlock,
  pushUserMessage, openPanel, closePanel,
} from '../store/sessions.js';

const MindView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.MindView })));
const ActivityView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.ActivityView })));
const ArtefactsView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.ArtefactsView })));

// Single source of truth for the views that exist.  TopBar's nav and
// the ⌘1..N keyboard handler both key off this list so adding/removing
// a view is a one-place change.
const VIEW_IDS = ['conv', 'mind', 'artefacts', 'activity'];

const MOBILE = '(max-width: 760px)';

// Hash routing — URLs are shareable across Tailscale nodes and the
// browser back button steps cleanly through them.  Two extras over
// the original shape:
//   * `#/c/<id>/t/<toolRef>` keeps a clicked tool chip in the URL so
//     reload / share lands on the same panel and back-button pops
//     the panel without leaving the chat.
//   * `#/mind/<path>` tracks the selected workspace file.
// `parseHash` accepts an explicit hash string so it's testable without
// poking at `window.location` from a unit test; production callers
// pass `window.location.hash`.
//   #/                              conv view, first conversation
//   #/c/<id>                        conv view, specific chat
//   #/c/<id>/t/<toolRef>            conv view, chat + tool panel open
//   #/mind                          mind view (no file selected)
//   #/mind/<encoded path>           mind view, specific file open
//   #/artefacts                     artefacts list
//   #/artefacts/<id>                reader open on that id
//   #/activity                      activity view
export function parseHash(rawArg) {
  const raw =
    rawArg !== undefined
      ? rawArg
      : (typeof window !== 'undefined' && window.location.hash) || '';
  const parts = raw.replace(/^#\/?/, '').split('/').filter(Boolean);
  const empty = { view: 'conv', conv: null, artefactId: null, toolRef: null, mindPath: null };
  if (!parts.length) return empty;
  if (parts[0] === 'c' && parts[1]) {
    return {
      ...empty,
      view: 'conv',
      conv: decodeURIComponent(parts[1]),
      toolRef: parts[2] === 't' && parts[3] ? decodeURIComponent(parts[3]) : null,
    };
  }
  if (parts[0] === 'artefacts' && parts[1])
    return { ...empty, view: 'artefacts', artefactId: decodeURIComponent(parts[1]) };
  if (parts[0] === 'mind' && parts.length >= 2) {
    // Workspace files can live in subdirs; rejoin with `/` so a
    // multi-segment path round-trips even if the encoder didn't pre-
    // encode the slash.  decodeURIComponent on each segment first.
    const path = parts.slice(1).map(decodeURIComponent).join('/');
    return { ...empty, view: 'mind', mindPath: path };
  }
  if (VIEW_IDS.includes(parts[0])) return { ...empty, view: parts[0] };
  return empty;
}

export function buildHash(state) {
  const { view, conv, artefactId, toolRef, mindPath } = state || {};
  if (view === 'conv') {
    if (!conv) return '#/';
    const base = `#/c/${encodeURIComponent(conv)}`;
    return toolRef ? `${base}/t/${encodeURIComponent(toolRef)}` : base;
  }
  if (view === 'artefacts')
    return artefactId ? `#/artefacts/${encodeURIComponent(artefactId)}` : '#/artefacts';
  if (view === 'mind')
    return mindPath ? `#/mind/${encodeURIComponent(mindPath)}` : '#/mind';
  return `#/${view}`;
}

function App() {
  const client = useApi();
  const initialRoute = parseHash();
  if (initialRoute.artefactId) requestOpenArtefact(initialRoute.artefactId);

  const [view, setView] = useState(initialRoute.view);
  const [conv, setConv] = useState(initialRoute.conv);
  // Tracked as state (not derived from hash) so the URL round-trips when
  // conv fills in from the deep-link's X-Dyson-Chat-Id response header —
  // otherwise the state→URL effect would clobber `#/artefacts/<id>` back
  // down to `#/artefacts` the instant the sidebar restored.
  const [artefactId, setArtefactId] = useState(initialRoute.artefactId);
  const [toolRef, setToolRef] = useState(initialRoute.toolRef);
  const [mindPath, setMindPath] = useState(initialRoute.mindPath);
  const selectView = useCallback((v) => {
    setView(v);
    setArtefactId(null);
    setToolRef(null);
    clearPendingArtefact();
  }, []);
  const [showLeft, setShowLeft] = useState(false);
  const [showRight, setShowRight] = useState(false);
  const [rightHidden, setRightHidden] = useState(false);

  const conversations = useAppState(s => s.conversations);
  const pendingArtefactId = useAppState(s => s.ui.pendingArtefactId);
  const openRailNonce = useAppState(s => s.ui.openRailNonce);

  // First live arrival: snap to the first conversation.  Only fires when
  // conv is empty so a deep-link's conv wins.
  useEffect(() => {
    if (!conv && conversations.length > 0) setConv(conversations[0].id);
  }, [conv, conversations]);

  // state → URL.  setState is a no-op when unchanged so no loop with the
  // popstate/hashchange listener below.
  useEffect(() => {
    const target = buildHash({
      view, conv,
      artefactId: view === 'artefacts' ? artefactId : null,
      toolRef:    view === 'conv'      ? toolRef    : null,
      mindPath:   view === 'mind'      ? mindPath   : null,
    });
    if (window.location.hash !== target) window.history.pushState(null, '', target);
  }, [view, conv, artefactId, toolRef, mindPath]);

  useEffect(() => {
    const h = () => {
      const r = parseHash();
      setView(r.view);
      if (r.conv != null) setConv(r.conv);
      setArtefactId(r.artefactId || null);
      setToolRef(r.toolRef || null);
      setMindPath(r.mindPath || null);
      if (r.artefactId) requestOpenArtefact(r.artefactId);
    };
    window.addEventListener('popstate', h);
    window.addEventListener('hashchange', h);
    return () => {
      window.removeEventListener('popstate', h);
      window.removeEventListener('hashchange', h);
    };
  }, []);

  // In-chat chip or deep-link → jump to the Artefacts tab with id set.
  useEffect(() => {
    if (!pendingArtefactId) return;
    setView('artefacts');
    setArtefactId(pendingArtefactId);
  }, [pendingArtefactId]);

  // ⌘1..N view switching (bounds-checked: pressing ⌘4/⌘5 once pointed
  // at deleted Providers/Sandbox views and grey-screened the app), ⌘K
  // for a new conversation.  ⌘N is claimed by the browser on macOS and
  // is not web-preventable, so we don't try to bind it.
  useEffect(() => {
    const h = (e) => {
      if (!(e.metaKey || e.ctrlKey)) return;
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        if (idx < VIEW_IDS.length) { e.preventDefault(); selectView(VIEW_IDS[idx]); }
      } else if (e.key === 'k') {
        e.preventDefault();
        client.createChat('New conversation').then(c => {
          upsertConversation({ id: c.id, title: c.title, live: false, source: 'http' });
          setConv(c.id);
        }).catch(() => {});
      }
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [client, selectView]);

  // iOS keyboard dodge.  On iOS the layout viewport is fixed when the
  // keyboard opens — only visualViewport shrinks — so a composer
  // anchored to the viewport bottom hides behind the keyboard.  Publish
  // the delta as --kb-inset and let composer-dock ride above it.  The
  // focus gate matters: Safari's URL-bar show/hide also shrinks the
  // visual viewport; without it the composer drifts into the middle of
  // the transcript when the URL bar is visible.
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const set = (px) => document.documentElement.style.setProperty('--kb-inset', px + 'px');
    const editing = () => {
      const a = document.activeElement;
      return !!a && (a.tagName === 'TEXTAREA' || a.tagName === 'INPUT' || a.isContentEditable);
    };
    const sync = () => set(editing() ? Math.max(0, window.innerHeight - vv.height - vv.offsetTop) : 0);
    vv.addEventListener('resize', sync);
    vv.addEventListener('scroll', sync);
    window.addEventListener('focusin', sync);
    window.addEventListener('focusout', sync);
    sync();
    return () => {
      vv.removeEventListener('resize', sync);
      vv.removeEventListener('scroll', sync);
      window.removeEventListener('focusin', sync);
      window.removeEventListener('focusout', sync);
    };
  }, []);

  const closeRails = useCallback(() => { setShowLeft(false); setShowRight(false); }, []);

  // Plug button is dual-purpose: on mobile it toggles the drawer, on
  // desktop it collapses the right column entirely.
  const onToggleRight = () => {
    if (window.matchMedia(MOBILE).matches) { setShowRight(s => !s); setShowLeft(false); }
    else setRightHidden(h => !h);
  };

  // ConversationView signals "open the right rail" by bumping a nonce.
  useEffect(() => {
    if (openRailNonce === 0) return;
    if (window.matchMedia(MOBILE).matches) { setShowRight(true); setShowLeft(false); }
    else setRightHidden(false);
  }, [openRailNonce]);

  const onToggleLeft = () => {
    // Artefacts tab has no LeftRail — hamburger drives the tree drawer
    // instead, otherwise mobile readers are a one-way door until users
    // find the back button inside the title bar.
    if (view === 'artefacts') { requestToggleArtefactsDrawer(); return; }
    setShowLeft(s => !s); setShowRight(false);
  };

  const bodyClass = `body${showLeft ? ' show-left' : ''}${showRight ? ' show-right' : ''}${rightHidden ? ' no-right' : ''}`;

  return (
    <div className="app">
      <TopBar view={view} setView={selectView} rightHidden={rightHidden}
              onToggleLeft={onToggleLeft} onToggleRight={onToggleRight}/>
      {view === 'conv' && (
        <main className={bodyClass}>
          {(showLeft || showRight) && <div className="scrim" onClick={closeRails}/>}
          <LeftRail active={conv} setActive={(id) => { setConv(id); setToolRef(null); setShowLeft(false); }}/>
          <ConversationView conv={conv} toolRef={toolRef} setToolRef={setToolRef}/>
          {!rightHidden && <RightRail chatId={conv}/>}
        </main>
      )}
      {view === 'mind' && (
        <main className="body no-left no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <Suspense fallback={<div/>}>
            <MindView
              showSide={showLeft}
              onHideSide={() => setShowLeft(false)}
              path={mindPath}
              setPath={setMindPath}/>
          </Suspense>
        </main>
      )}
      {view === 'artefacts' && (
        <main className="body no-left no-right">
          <Suspense fallback={<div/>}><ArtefactsView conv={conv} setConv={setConv}/></Suspense>
        </main>
      )}
      {view === 'activity' && (
        <main style={{display:'flex', flex:1, minHeight:0}}>
          <Suspense fallback={<div/>}><ActivityView/></Suspense>
        </main>
      )}
    </div>
  );
}

// Renders the session for `conv`.  Streaming SSE for inactive chats
// keeps populating their sessions, so switching back picks up exactly
// where the user left off.
function ConversationView({ conv, toolRef, setToolRef }) {
  const client = useApi();
  const session = useSession(conv);
  const mutate = useSessionMutator(conv);
  const tools = useAppState(s => s.tools);
  const conversations = useAppState(s => s.conversations);
  const scrollRef = useRef(null);

  // URL → state: when the hash points at a specific tool ref (deep-
  // link, back-button restore), open the matching panel as soon as
  // the session is hydrated.  When the URL drops the suffix (back-
  // button popping the panel out of history), close the panel so the
  // back-button is symmetric with the click that opened it.
  useEffect(() => {
    if (!conv || !session?.loaded) return;
    if (toolRef) {
      if (session.openTool !== toolRef) {
        mutate(s => openPanel(s, toolRef));
        requestOpenRail();
      }
    } else if (session.openTool && session.panels.includes(session.openTool)) {
      // URL no longer carries a tool ref — pop the panel.
      mutate(s => closePanel(s, s.openTool));
    }
  }, [conv, toolRef, session?.loaded]);

  // First open of this conv in this tab: hydrate from the API.
  // Subsequent switches are no-ops because session.loaded flips on
  // first entry.
  useEffect(() => {
    if (!conv) return;
    ensureSession(conv);
    if (getSession(conv)?.loaded) {
      updateSession(conv, s => s.justScrollOnNextRender ? s : { ...s, justScrollOnNextRender: true });
      return;
    }
    updateSession(conv, s => ({ ...s, loaded: true, justScrollOnNextRender: true }));
    client.loadFeedback(conv).then(entries => {
      const ratings = {};
      for (const e of (entries || [])) ratings[e.turn_index] = RATING_EMOJI[e.rating] || '';
      updateSession(conv, s => ({ ...s, ratings }));
    }).catch(() => {});
    client.load(conv).then(data => {
      hydrateTranscript(conv, data);
      // If the chat is currently generating on the server, re-attach
      // the SSE stream so deltas keep flowing into the UI.  Without
      // this the page paints whatever's on disk and then sits there
      // mute while the agent runs to completion in the background —
      // the original "mid-stream reload blanks the chat" symptom.
      if (data && data.live) {
        getResources(conv).es = attachLiveStream(conv, client);
      }
    }).catch(() => {});
  }, [conv, client]);

  // Auto-scroll.  Pin to bottom on first entry so chats open at the
  // latest message; "near-bottom only" for deltas so users scrolled up
  // to read older context don't get yanked down.  Layout effect because
  // a paint-then-scroll sequence briefly shows the top of the
  // transcript and reads as jank.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || !session) return;
    if (session.justScrollOnNextRender && session.liveTurns.length > 0) {
      el.scrollTop = el.scrollHeight;
      updateSession(conv, s => s.justScrollOnNextRender ? { ...s, justScrollOnNextRender: false } : s);
      return;
    }
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 240) el.scrollTop = el.scrollHeight;
  });

  const handleOpenTool = (ref) => {
    if (!session) return;
    // Tap an already-open chip to close its panel — drop the URL
    // suffix too so the back button doesn't reopen what the user just
    // closed.
    if (session.openTool === ref && session.panels.includes(ref)) {
      mutate(s => closePanel(s, ref));
      if (typeof setToolRef === 'function') setToolRef(null);
      return;
    }
    mutate(s => openPanel(s, ref));
    requestOpenRail();
    // Push the tool into the URL so reload / share / back-button
    // round-trips it.
    if (typeof setToolRef === 'function') setToolRef(ref);
  };

  // Dismiss the reaction bar when the user taps outside any open turn.
  // One document listener because only one bar is open at a time.
  const openRating = session?.openRating;
  useEffect(() => {
    if (!conv || openRating == null) return;
    const h = (e) => {
      if (!e.target.closest('.turn.reactions-open')) {
        mutate(s => s.openRating == null ? s : { ...s, openRating: null });
      }
    };
    document.addEventListener('pointerdown', h);
    return () => document.removeEventListener('pointerdown', h);
  }, [conv, openRating, mutate]);

  const sendMsg = async (val, files) => {
    // No conversation selected (fresh dyson, user typed straight into
    // the input without clicking "+ New conversation" first).  Auto-
    // create one and proceed.  Pre-fix this branch silently returned
    // and the user's send-button click went nowhere — no fetch, no
    // visible feedback, indistinguishable from a frontend hang.
    let activeConv = conv;
    if (!activeConv) {
      try {
        const c = await client.createChat('New conversation');
        upsertConversation({ id: c.id, title: c.title, live: false, source: 'http' });
        setConv(c.id);
        activeConv = c.id;
      } catch (e) {
        console.warn('[dyson] auto-create chat failed', e);
        return;
      }
    }
    // /clear is controller-side — the server rotates the chat file and
    // clears agent context.  Skip the optimistic turn pair (no LLM
    // reply is coming) and reset local state once the POST lands.
    if (val.trim() === '/clear' && !(files && files.length)) {
      fetch(`/api/conversations/${encodeURIComponent(activeConv)}/turn`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ prompt: '/clear' }),
      }).then(r => {
        if (!r.ok) return;
        mutate(s => ({
          ...s, liveTurns: [], artefacts: [], panels: [],
          openTool: null, thinkingRef: null, liveToolRef: null,
        }));
      });
      return;
    }

    const ts = new Date().toTimeString().slice(0, 8);
    const userBlocks = [{ type: 'text', text: val }, ...(files || []).map(f => ({
      type: 'file', name: f.name,
      mime: f.type || 'application/octet-stream',
      size: f.size,
      url: f.type?.startsWith('image/') ? URL.createObjectURL(f) : null,
      local: true,
    }))];
    // The merge-or-push decision lives in `pushUserMessage` so it can
    // be unit-tested as a pure reducer.  The server coalesces every
    // drained queued turn into one `agent.run()`; the SPA mirrors
    // that by merging consecutive queued sends into one bubble.
    mutate(s => pushUserMessage(s, { ts, blocks: userBlocks }));
    getResources(activeConv).es = client.send(activeConv, val, streamCallbacks(activeConv), files);
  };

  const onCancel = () => {
    if (conv) client.cancel(conv).catch(() => {});
    const r = getResources(conv);
    if (r.es) { try { r.es.close(); } catch { /* already closed */ } r.es = null; }
    mutate(s => s.running ? { ...s, running: false } : s);
  };

  const onRate = (turnIndex, emoji) => {
    if (!conv) return;
    const prev = session?.ratings[turnIndex];
    mutate(s => setRating(s, turnIndex, emoji));
    client.feedback(conv, turnIndex, emoji).catch(() => {
      mutate(s => setRating(s, turnIndex, prev));
    });
  };

  const toggleReactions = (turnIndex) =>
    mutate(s => ({ ...s, openRating: s.openRating === turnIndex ? null : turnIndex }));

  if (!session) return <ConversationShell onSend={noop} onCancel={noop}/>;

  const title = conversations.find(c => c.id === conv)?.title || conv || '';
  const empty = session.liveTurns.length === 0 && !session.running;

  const onExport = () => client.exportConversation(conv).then(downloadBlob(`${conv}.sharegpt.json`))
    .catch(e => { console.warn('[dyson] export failed', e); alert(`Export failed: ${e.message || e}`); });

  return (
    <div className={`centre${empty ? ' empty' : ''}`}>
      <div className="aurora-sweep" aria-hidden="true"/>
      <div className="context">
        <div className="crumbs"><span className="c-leaf">{title}</span></div>
        <div className="right">
          <button className="btn sm ghost" title="Download ShareGPT export"
                  onClick={onExport} disabled={!conv} style={{padding:'4px 8px'}}>
            <Icon name="download" size={13}/>
          </button>
        </div>
      </div>
      <div className="transcript" ref={scrollRef}>
        <div className="inner">
          {empty ? <EmptyState/> : session.liveTurns.map((t, i) => (
            <Turn key={i} turn={t} tools={tools}
                  onOpenTool={handleOpenTool} activeTool={session.openTool}
                  turnIndex={i} rating={session.ratings[i]} onRate={onRate}
                  reactionsOpen={session.openRating === i}
                  onToggleReactions={() => toggleReactions(i)}/>
          ))}
        </div>
      </div>
      <ComposerDock running={session.running} phase={session.phase} tname={session.tname}
                    onJump={() => session.liveToolRef && handleOpenTool(session.liveToolRef)}
                    onSend={sendMsg} onCancel={onCancel}/>
    </div>
  );
}

const noop = () => {};

// Default shell painted before the session hydrates (or when no chat
// exists yet).  Keeps the app frame on screen during cold load instead
// of a blank white pane.
function ConversationShell({ onSend, onCancel, children }) {
  return (
    <div className="centre empty">
      <div className="aurora-sweep" aria-hidden="true"/>
      <div className="context"><div className="crumbs"/></div>
      <div className="transcript"><div className="inner">{children || <EmptyState/>}</div></div>
      <ComposerDock running={false} onSend={onSend} onCancel={onCancel}/>
    </div>
  );
}

function ComposerDock({ running, phase, tname, onJump, onSend, onCancel }) {
  return (
    <div className="composer-dock">
      <div style={{width:'100%',maxWidth:820,display:'flex',flexDirection:'column',alignItems:'stretch'}}>
        {running && <TypingIndicator phase={phase} tname={tname} onJump={onJump}/>}
        <Composer onSend={onSend} onCancel={onCancel} running={!!running}/>
      </div>
    </div>
  );
}

const RATING_EMOJI = {
  terrible: '💩', bad: '👎', not_good: '😐',
  good: '👍', very_good: '🔥', excellent: '❤️',
};

function setRating(s, turnIndex, emoji) {
  const next = { ...s.ratings };
  if (emoji) next[turnIndex] = emoji; else delete next[turnIndex];
  return { ...s, ratings: next };
}

function downloadBlob(filename) {
  return (blob) => {
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url; a.download = filename;
    document.body.appendChild(a); a.click(); a.remove();
    URL.revokeObjectURL(url);
  };
}

// -- SSE callbacks ------------------------------------------------------
// Each callback is a reducer over the session store (for transcript +
// panels) or the tool store (for tool views shared across components).
// Helpers live in store/sessions.js so the flow is composable: every
// mutation returns a new frozen snapshot.
function streamCallbacks(conv) {
  const appendToolText = (ref, extra) => updateTool(ref,
    t => ({ ...t, body: { ...t.body, text: (t.body.text || '') + extra } }));

  return {
    onText: (delta) => updateSession(conv, s => mapAgentTail(s, t => {
      const tail = t.blocks[t.blocks.length - 1];
      return tail?.type === 'text'
        ? { ...t, blocks: [...t.blocks.slice(0, -1), { ...tail, text: tail.text + delta }] }
        : { ...t, blocks: [...t.blocks, { type: 'text', text: delta }] };
    })),

    onThinking: (delta) => {
      const existing = getSession(conv)?.thinkingRef;
      if (existing) return appendToolText(existing, delta);
      // One thinking panel per turn; all deltas land in the same ref.
      const ref = mintToolRef(conv, 'thinking');
      setTool(ref, mkTool('thinking', {
        kind: 'thinking', status: 'running', dur: '…', body: { text: delta },
      }));
      updateSession(conv, s => ({
        ...openPanel(appendAgentBlock(s, { type: 'tool', ref }), ref),
        thinkingRef: ref,
      }));
    },

    onToolStart: ({ id, name, parent_tool_id }) => {
      // Nested subagent tool call — attach to the parent panel as a
      // child chip instead of minting a new top-level chip.  Drop on
      // the floor if the parent panel isn't mounted (race on reload);
      // the user will still see the subagent's outer chip, just not
      // the nested progress.
      if (parent_tool_id) {
        updateTool(parent_tool_id, t => {
          const existing = Array.isArray(t.body?.children) ? t.body.children : [];
          const child = { id: id || `child-${existing.length + 1}`, name, status: 'running', dur: '…' };
          return { ...t, kind: 'subagent', body: { ...(t.body || {}), children: [...existing, child] } };
        });
        return;
      }
      const ref = id || mintToolRef(conv, 'live');
      setTool(ref, mkTool(name, { status: 'running', dur: '…' }));
      updateSession(conv, s => ({
        ...openPanel(appendAgentBlock(s, { type: 'tool', ref }), ref),
        phase: 'tool', tname: name, liveToolRef: ref,
      }));
    },

    onToolResult: ({ content, is_error, view, parent_tool_id, tool_use_id }) => {
      // Nested subagent tool result — find the matching child by id
      // (the backend always tags nested results with `tool_use_id`,
      // including for parallel dispatches).  No fallback: a missing
      // id or an unmatched id means we can't safely route the result,
      // so we drop it rather than guess and corrupt the wrong child.
      if (parent_tool_id) {
        if (!tool_use_id) return;
        updateTool(parent_tool_id, t => {
          const children = Array.isArray(t.body?.children) ? t.body.children : [];
          const idx = children.findIndex(c => c.id === tool_use_id);
          if (idx < 0) return t;
          // Strip the `kind` field out of `view` (it lives on the entry,
          // not the body) without an IIFE allocation.
          let body;
          let kind = children[idx].kind;
          if (view) {
            const { kind: viewKind, ...rest } = view;
            kind = viewKind;
            body = rest;
          } else {
            body = { text: content };
          }
          const nextChildren = children.slice();
          nextChildren[idx] = { ...children[idx],
            status: 'done',
            exit: is_error ? 'err' : 'ok',
            kind, body,
          };
          return { ...t, body: { ...t.body, children: nextChildren } };
        });
        return;
      }
      const ref = getSession(conv)?.liveToolRef;
      if (!ref) return;
      // image_generate's content is `Generated N image(s) for: "…"` —
      // capture the prompt so ImagePanel can surface it alongside the
      // picture.  File event arrives later and overwrites the body.
      const prompt = typeof content === 'string'
        ? content.match(/^Generated \d+ image\(s\) for: "(.+)"$/s)?.[1]
        : null;
      updateTool(ref, t => {
        const next = applyToolView(t, content, is_error, view);
        return prompt && next.name === 'image_generate' ? { ...next, prompt } : next;
      });
    },

    onCheckpoint: ({ text }) => {
      const ref = getSession(conv)?.liveToolRef;
      if (ref) appendToolText(ref, text + '\n');
    },

    onError: (message) => updateSession(conv,
      s => appendAgentBlock(s, { type: 'error', message })),

    onFile: ({ name, mime_type, url, inline_image }) => {
      updateSession(conv, s => appendAgentBlock(s, {
        type: 'file', name, mime: mime_type, url, inline: !!inline_image,
      }));
      // For images: also flip the active tool panel into image-kind so
      // the right-rail shows the picture without scrolling chat.
      if (inline_image) {
        const ref = getSession(conv)?.liveToolRef;
        if (ref) updateTool(ref, t => ({
          ...t, kind: 'image',
          body: { url, name, mime: mime_type, prompt: t.prompt || '' },
        }));
      }
    },

    onArtefact: ({ id, kind, title, url, bytes, metadata }) => {
      updateSession(conv, s => {
        // Image artefacts double up on screen during live turns — the
        // preceding `file` event already rendered an <img>.  Skip the
        // transcript chip for images; still track in session.artefacts
        // so the Artefacts tab lists them.
        const withBlock = kind === 'image'
          ? s
          : appendAgentBlock(s, { type: 'artefact', id, kind, title, url, bytes });
        return {
          ...withBlock,
          artefacts: [
            { id, kind, title, bytes, created_at: Math.floor(Date.now() / 1000), metadata },
            ...s.artefacts,
          ],
        };
      });
      markConversationHasArtefacts(conv);
    },

    onDone: () => {
      const ref = getSession(conv)?.thinkingRef;
      if (ref) updateTool(ref, t => ({ ...t, status: 'done', dur: '' }));
      // Mark that any subsequent delta belongs to a new agent run --
      // this is the queue-drain case where the server processes the
      // next queued message right after this Done.  `mapAgentTail`
      // mints a fresh agent turn when it sees `nextAgentNew`, so the
      // drain reply doesn't graft onto the just-finished turn.
      updateSession(conv, s => s.running
        ? { ...s, running: false, thinkingRef: null, nextAgentNew: true }
        : s);
      getResources(conv).es = null;
    },
  };
}

// -- Transcript hydration ----------------------------------------------
// Consumes the /api/conversations/:id response, seeds the session store
// and the tool view dict, and handles the retroactive image-artefact
// pairing legacy chats need (image artefacts emitted before the server
// tracked tool_use_id get paired positionally with image_generate tool
// panels).
function hydrateTranscript(conv, data) {
  const mintCounter = () => { const r = getResources(conv); r.counter += 1; return r.counter; };
  const tools = {};

  const blockOf = (b) => {
    switch (b.type) {
      case 'text':     return { type: 'text', text: b.text };
      case 'thinking': return { type: 'thinking', text: b.thinking };
      case 'file':     return { type: 'file', name: b.name, mime: b.mime, size: b.bytes, url: b.url, inline: !!b.inline_image };
      case 'tool_use': {
        const id = b.id || `${conv}-tu-${mintCounter()}`;
        tools[id] = tools[id] || mkTool(b.name);
        if (b.name === 'image_generate' && b.input?.prompt) tools[id].prompt = b.input.prompt;
        return { type: 'tool', ref: id };
      }
      case 'tool_result': {
        const t = tools[b.tool_use_id];
        if (t) Object.assign(t, { status: 'done', exit: b.is_error ? 'err' : 'ok', body: { text: b.content } });
        return null;
      }
      case 'artefact': {
        // Image artefact with a known tool_use_id: flip the matching
        // tool panel into image-kind so the right-rail shows the
        // picture instead of text.
        if (b.kind === 'image' && b.tool_use_id && tools[b.tool_use_id]) {
          const t = tools[b.tool_use_id];
          t.kind = 'image';
          t.body = {
            url: b.metadata?.file_url || b.url,
            name: b.metadata?.file_name || b.title,
            mime: 'image/*',
            prompt: t.prompt || '',
          };
        }
        return { type: 'artefact', id: b.id, kind: b.kind, title: b.title, url: b.url, bytes: b.bytes, tool_use_id: b.tool_use_id, metadata: b.metadata };
      }
      default: return null;
    }
  };

  const liveTurns = (data.messages || [])
    .map(m => ({ role: m.role === 'user' ? 'user' : 'agent', ts: '', blocks: m.blocks.map(blockOf).filter(Boolean) }))
    .filter(t => t.blocks.length > 0);

  // Retroactive pairing: pre-tool_use_id chats have orphan image
  // artefacts.  Pair by position with unfilled image_generate panels.
  const allBlocks = liveTurns.flatMap(t => t.blocks);
  const orphans = allBlocks.filter(b => b.type === 'artefact' && b.kind === 'image' && !b.tool_use_id);
  const unfilled = allBlocks.filter(b => b.type === 'tool' && tools[b.ref]?.name === 'image_generate' && tools[b.ref]?.kind !== 'image');
  const pending = [];
  for (let i = 0; i < Math.min(orphans.length, unfilled.length); i += 1) {
    const art = orphans[i], tool = tools[unfilled[i].ref];
    tool.kind = 'image';
    tool.body = {
      url: art.metadata?.file_url || '',
      name: art.metadata?.file_name || art.title,
      mime: 'image/*',
    };
    if (!tool.body.url) pending.push({ ref: unfilled[i].ref, url: art.url });
  }

  mergeTools(tools);
  const artefacts = allBlocks
    .filter(b => b.type === 'artefact')
    .map(b => ({ id: b.id, kind: b.kind, title: b.title, bytes: b.bytes, created_at: 0, metadata: b.metadata }));
  updateSession(conv, s => ({ ...s, liveTurns, artefacts }));

  // Resolve orphan image URLs out-of-band (older chats store the file
  // URL as the artefact body text).
  for (const { ref, url } of pending) {
    fetch(url)
      .then(r => r.ok ? r.text() : Promise.reject(r.status))
      .then(text => updateTool(ref, t => ({ ...t, body: { ...t.body, url: text.trim() } })))
      .catch(() => {});
  }
}

// Open the SSE stream for an in-flight chat without POSTing a turn,
// and prepare the local session to receive the deltas.  Called by
// the hydrate effect when `data.live` is true (mid-stream reload).
//
// Steps:
//   1. Append an empty agent placeholder iff the last turn isn't
//      already an agent turn — onText appends to the last block of
//      the last turn, so it needs an agent target to write into.
//   2. Flip running so the typing indicator paints.
//   3. Open the SSE stream via client.attach (no /turn POST → no 409).
function attachLiveStream(conv, client) {
  updateSession(conv, s => {
    const last = s.liveTurns[s.liveTurns.length - 1];
    const liveTurns = last && last.role === 'agent'
      ? s.liveTurns
      : [...s.liveTurns, { role: 'agent', ts: '', blocks: [{ type: 'text', text: '' }] }];
    return { ...s, liveTurns, running: true, phase: 'thinking' };
  });
  return client.attach(conv, streamCallbacks(conv));
}

function mkTool(name, over = {}) {
  return {
    name,
    icon: ((name && name[0]) || '?').toUpperCase(),
    sig: '', dur: '', exit: 'ok', status: 'done', kind: 'fallback',
    body: { text: '' },
    ...over,
  };
}

// Apply a typed ToolView (SSE wire shape) to a tool entry, producing a
// new frozen value.  Pure — called as an updateTool reducer argument.
//
// Subagent special case: when the panel was already flipped to
// kind='subagent' by inner tool_use_start events streaming in during
// the subagent's run, preserve `body.children` and stash the final
// summary text as `body.summary`.  Without this guard, the typed view
// (or the fallback `{ text: content }`) would clobber the live list of
// child chips users have been watching populate for minutes.
function applyToolView(t, content, isError, view) {
  const base = { ...t, status: 'done', exit: isError ? 'err' : 'ok' };
  if (t.kind === 'subagent' && Array.isArray(t.body?.children)) {
    return { ...base, body: { ...t.body, summary: content } };
  }
  if (!view?.kind) return { ...base, kind: 'fallback', body: { text: content } };
  const { kind: _k, ...body } = view;
  const next = { ...base, kind: view.kind, body };
  if (view.kind === 'bash' && typeof view.duration_ms === 'number') {
    next.dur = view.duration_ms < 1000
      ? `${view.duration_ms}ms`
      : `${(view.duration_ms / 1000).toFixed(1)}s`;
  }
  if (view.kind === 'diff' && view.files?.[0]) {
    next.sig = view.files[0].path;
    next.meta = `+${view.files[0].add} −${view.files[0].rem}`;
  }
  if (view.kind === 'read' && view.path) next.sig = view.path;
  return next;
}

export { App, streamCallbacks, attachLiveStream };
