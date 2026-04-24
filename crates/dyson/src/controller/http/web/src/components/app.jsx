/* Dyson — app root.  Live mode only; no seed-data branches.
 *
 * Per-chat state lives in the sessions store (../store/sessions.js).
 * Switching `conv` does NOT clear the prior chat's transcript, panels,
 * ratings, running flag, or in-flight EventSource — each chat's session
 * stays in the store's frozen map and surfaces again when the user
 * switches back.  This is the fix for "moving from a chat seems to kill
 * it" and "the tool stack is not per conversation".
 *
 * The old version held sessions as a React ref to a plain Map of plain
 * objects and mutated fields in place, re-rendering via a `bump()`
 * counter.  That pattern silently dropped renders when React's
 * reconciler compared identical object references and skipped the
 * subtree.  The whole refactor exists to replace it: snapshots are
 * frozen, every update produces a new reference, and useSyncExternalStore
 * drives re-renders per-slice rather than per-tree.
 */

import React, { useState, useEffect, useRef, useCallback, useLayoutEffect, Suspense, lazy } from 'react';
import { Icon } from './icons.jsx';
import {
  Turn, Composer, TypingIndicator, EmptyState,
} from './turns.jsx';
import { TopBar, LeftRail, RightRail } from './views.jsx';
import { useApi } from '../hooks/useApi.js';
import { useAppState } from '../hooks/useAppState.js';
import { useSession, useSessionMutator } from '../hooks/useSession.js';
import {
  selectConversations, selectTools,
  setTool, updateTool, upsertConversation,
  markConversationHasArtefacts,
  requestOpenRail, requestOpenArtefact, clearPendingArtefact,
} from '../store/app.js';
import { app } from '../store/app.js';
import {
  ensureSession, updateSession, getSession, getResources, mintToolRef,
} from '../store/sessions.js';

// Mind / Activity / Artefacts aren't on the cold-load route — split
// them into their own chunk so the initial bundle only carries the
// conversation shell.
const MindView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.MindView })));
const ActivityView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.ActivityView })));
const ArtefactsView = lazy(() =>
  import('./views-secondary.jsx').then(m => ({ default: m.ArtefactsView })));

// Single source of truth for the views that exist.  TopBar's nav array
// must list these in the same order; the keyboard handler in App keys
// off this list so adding/removing a view requires updating only one
// place.
const VIEW_IDS = ['conv', 'mind', 'artefacts', 'activity'];

// Hash-based router: URLs are shareable (send the link to another
// Tailscale node and they land on the same conversation / view).
//   #/                    → conv view, default to the first conversation
//   #/c/<chat_id>         → conv view, specific chat
//   #/mind                → mind view
//   #/artefacts           → artefacts view (list)
//   #/artefacts/<id>      → artefacts view, reader open on that id
//   #/activity            → activity view
// Hash keeps us out of the server's routing — `/`, `/c/id`, `/mind`
// all serve the same SPA shell and the client reads the fragment.
function parseHash() {
  const raw = (typeof window !== 'undefined' && window.location.hash) || '';
  const parts = raw.replace(/^#\/?/, '').split('/').filter(Boolean);
  if (!parts.length) return { view: 'conv', conv: null, artefactId: null };
  if (parts[0] === 'c' && parts[1]) {
    return { view: 'conv', conv: decodeURIComponent(parts[1]), artefactId: null };
  }
  if (parts[0] === 'artefacts' && parts[1]) {
    return { view: 'artefacts', conv: null, artefactId: decodeURIComponent(parts[1]) };
  }
  if (VIEW_IDS.includes(parts[0])) return { view: parts[0], conv: null, artefactId: null };
  return { view: 'conv', conv: null, artefactId: null };
}

function buildHash(view, conv, artefactId) {
  if (view === 'conv') return conv ? `#/c/${encodeURIComponent(conv)}` : '#/';
  if (view === 'artefacts' && artefactId) return `#/artefacts/${encodeURIComponent(artefactId)}`;
  return `#/${view}`;
}

function App() {
  const client = useApi();
  const initialRoute = parseHash();
  // Prime the store with the deep-link target on first render so
  // ArtefactsView's mount sees it synchronously.
  if (initialRoute.artefactId) requestOpenArtefact(initialRoute.artefactId);

  const [view, setView] = useState(initialRoute.view);
  const [conv, setConv] = useState(initialRoute.conv);
  // The id in `#/artefacts/<id>`, tracked as state so the URL
  // round-trips when conv gets filled in from the deep-link's response
  // header (otherwise the state→URL effect would clobber `#/artefacts/a0`
  // back down to `#/artefacts` the moment we restore the sidebar).
  const [artefactId, setArtefactId] = useState(initialRoute.artefactId);
  // Tab-click entry point into the view nav: clears the artefact
  // deep-link so a stale id doesn't re-stick when the user taps the
  // Artefacts tab after coming from `#/artefacts/<id>`.  Deep-link
  // and chip-click paths bypass this and set `artefactId` explicitly.
  const selectView = useCallback((v) => {
    setView(v);
    setArtefactId(null);
    clearPendingArtefact();
  }, []);
  const [showLeft, setShowLeft] = useState(false);
  const [showRight, setShowRight] = useState(false);
  const [rightHidden, setRightHidden] = useState(false);

  const conversations = useAppState(selectConversations);
  const pendingArtefactId = useAppState(s => s.ui.pendingArtefactId);
  const openRailNonce = useAppState(s => s.ui.openRailNonce);

  // First live arrival: snap to the first conversation.  Only fires
  // when conv is empty so a deep-link's conv wins.
  useEffect(() => {
    if (!conv && conversations.length > 0) setConv(conversations[0].id);
  }, [conv, conversations]);

  // state → URL: push a new hash entry when view or conv changes so
  // back/forward walk through the user's navigation history.  Skip the
  // push when the hash already matches to avoid redundant entries
  // (first mount, bounce from URL → state → URL).
  useEffect(() => {
    const target = buildHash(view, conv, view === 'artefacts' ? artefactId : null);
    if (window.location.hash !== target) {
      window.history.pushState(null, '', target);
    }
  }, [view, conv, artefactId]);

  // URL → state: popstate fires on back/forward; hashchange covers
  // manual address-bar edits and shared links pasted in.  Parse and
  // sync — setState is a no-op when the value is unchanged, so no
  // loop with the state → URL effect above.
  useEffect(() => {
    const h = () => {
      const r = parseHash();
      setView(r.view);
      if (r.conv != null) setConv(r.conv);
      setArtefactId(r.artefactId || null);
      if (r.artefactId) requestOpenArtefact(r.artefactId);
    };
    window.addEventListener('popstate', h);
    window.addEventListener('hashchange', h);
    return () => {
      window.removeEventListener('popstate', h);
      window.removeEventListener('hashchange', h);
    };
  }, []);

  // In-chat artefact chip or deep-link → flip to the Artefacts tab and
  // remember which artefact to show.  The store's pendingArtefactId is
  // the payload; we clear it from the pickup side so only the
  // triggering navigation sees each signal.
  useEffect(() => {
    if (!pendingArtefactId) return;
    setView('artefacts');
    setArtefactId(pendingArtefactId);
  }, [pendingArtefactId]);

  // ⌘1..N view switching (bounds-checked against VIEW_IDS — pressing
  // ⌘4/⌘5 used to point at the deleted Providers/Sandbox views and
  // grey-screen the app), ⌘K for new conversation.  ⌘N is claimed by
  // the browser (opens a new window in Chrome/Safari/Firefox on macOS
  // and is not web-preventable) so we don't try to bind it.
  useEffect(() => {
    const h = (e) => {
      if (!(e.metaKey || e.ctrlKey)) return;
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        if (idx < VIEW_IDS.length) {
          e.preventDefault();
          selectView(VIEW_IDS[idx]);
        }
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

  // iOS keyboard dodge.  On iOS the layout viewport stays fixed when
  // the on-screen keyboard opens — only visualViewport shrinks — so a
  // composer anchored to the viewport bottom ends up hidden behind the
  // keyboard.  Publish the delta as --kb-inset while an editable field
  // is focused; composer-dock reads it and rides above the keyboard.
  // The focus gate matters because Safari's URL-bar show/hide also
  // shrinks visualViewport, and without the gate the composer drifts
  // into the middle of the transcript when the URL bar is visible.
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const set = (px) => document.documentElement.style.setProperty('--kb-inset', px + 'px');
    const isEditing = () => {
      const a = document.activeElement;
      return !!a && (a.tagName === 'TEXTAREA' || a.tagName === 'INPUT' || a.isContentEditable);
    };
    const sync = () => {
      if (!isEditing()) { set(0); return; }
      set(Math.max(0, window.innerHeight - vv.height - vv.offsetTop));
    };
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

  // Plug button is dual-purpose: on mobile it opens the drawer, on
  // desktop it collapses the column.  Detected at click time.
  const onToggleRight = () => {
    if (window.matchMedia('(max-width: 760px)').matches) {
      setShowRight(s => !s); setShowLeft(false);
    } else {
      setRightHidden(h => !h);
    }
  };

  // ConversationView signals "open the right rail" by bumping the
  // openRailNonce.  Force the rail open in whichever mode applies.
  useEffect(() => {
    if (openRailNonce === 0) return;
    if (window.matchMedia('(max-width: 760px)').matches) {
      setShowRight(true); setShowLeft(false);
    } else {
      setRightHidden(false);
    }
  }, [openRailNonce]);

  const session = useSession(conv);
  const bodyClass = `body${showLeft ? ' show-left' : ''}${showRight ? ' show-right' : ''}${rightHidden ? ' no-right' : ''}`;

  const removePanel = useCallback((ref) => {
    if (!conv) return;
    updateSession(conv, s => s.panels.includes(ref)
      ? { ...s, panels: s.panels.filter(x => x !== ref) }
      : s);
  }, [conv]);

  return (
    <div className="app">
      <TopBar view={view} setView={selectView}
              rightHidden={rightHidden}
              onToggleLeft={() => {
                // On the Artefacts tab the LeftRail is gone, so the
                // hamburger drives the single-sidebar tree drawer
                // instead — without this the mobile reader is a
                // one-way door until the user finds the back button
                // inside the title bar.
                if (view === 'artefacts') {
                  // Toggle via the store — ArtefactsView subscribes.
                  app.dispatch(s => ({
                    ...s,
                    ui: { ...s.ui, toggleArtefactsDrawerNonce: s.ui.toggleArtefactsDrawerNonce + 1 },
                  }));
                  return;
                }
                setShowLeft(s => !s); setShowRight(false);
              }}
              onToggleRight={onToggleRight}/>
      {view === 'conv' && (
        <main className={bodyClass}>
          {(showLeft || showRight) && <div className="scrim" onClick={closeRails}/>}
          <LeftRail active={conv} setActive={(id) => { setConv(id); setShowLeft(false); }}/>
          <ConversationView conv={conv}/>
          {!rightHidden && <RightRail panels={session ? session.panels : []} onClose={removePanel} activeChatId={conv}/>}
        </main>
      )}
      {view === 'mind' && (
        <main className="body no-left no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <Suspense fallback={<div/>}>
            <MindView showSide={showLeft} onHideSide={() => setShowLeft(false)}/>
          </Suspense>
        </main>
      )}
      {view === 'artefacts' && (
        <main className="body no-left no-right">
          {/* Single-sidebar layout: the drawer shows every chat with
              artefacts as a collapsible tree.  No LeftRail — it was an
              unreachable duplicate on mobile and a two-step navigation
              on desktop. */}
          <Suspense fallback={<div/>}>
            <ArtefactsView conv={conv} setConv={setConv}/>
          </Suspense>
        </main>
      )}
      {view === 'activity' && (
        <main style={{display:'flex', flex:1, minHeight:0}}>
          <Suspense fallback={<div/>}>
            <ActivityView/>
          </Suspense>
        </main>
      )}
    </div>
  );
}

// Per-chat state lives in the sessions store.  This component is a
// renderer over the session for the active `conv`; switching `conv`
// swaps which session it reads from but does NOT mutate any session it
// leaves.  Streaming SSE for inactive chats keeps populating their
// session, so switching back picks up exactly where we left off.
function ConversationView({ conv }) {
  const client = useApi();
  const session = useSession(conv);
  const mutate = useSessionMutator(conv);
  const tools = useAppState(selectTools);
  const conversations = useAppState(selectConversations);
  const scrollRef = useRef(null);

  // First time this conv is opened in this tab, hydrate from the API
  // (transcript + ratings).  Subsequent switches are no-ops because
  // session.loaded flips on first entry.
  useEffect(() => {
    if (!conv) return;
    ensureSession(conv);
    const current = getSession(conv);
    if (current && current.loaded) {
      updateSession(conv, s => s.justScrollOnNextRender ? s : { ...s, justScrollOnNextRender: true });
      return;
    }
    updateSession(conv, s => ({ ...s, loaded: true, justScrollOnNextRender: true }));
    const ratingToEmoji = {
      terrible: '💩', bad: '👎', not_good: '😐',
      good: '👍', very_good: '🔥', excellent: '❤️',
    };
    client.loadFeedback(conv).then(entries => {
      const map = {};
      for (const e of (entries || [])) map[e.turn_index] = ratingToEmoji[e.rating] || '';
      updateSession(conv, s => ({ ...s, ratings: map }));
    }).catch(() => {});
    client.load(conv).then(data => hydrateTranscript(conv, data)).catch(() => {});
  }, [conv, client]);

  // Auto-scroll: pin to the bottom the first time we render content
  // for this conv (so opening a chat drops the user at the latest
  // message instead of at the top and then scrolling down), then
  // "near-bottom only" for subsequent streaming deltas — don't yank
  // a user who has scrolled up to read older context.  Runs inside
  // `useLayoutEffect` so the scroll lands BEFORE the browser paints
  // the new DOM: without that, switching chats briefly showed the
  // transcript's top and then jumped down, which read as jank.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || !session) return;
    if (session.justScrollOnNextRender && session.liveTurns.length > 0) {
      el.scrollTop = el.scrollHeight;
      updateSession(conv, s => s.justScrollOnNextRender ? { ...s, justScrollOnNextRender: false } : s);
      return;
    }
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 240;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  });

  const handleOpenTool = (ref) => {
    if (!session) return;
    // Toggle: clicking an already-open tool chip closes its panel.
    if (session.openTool === ref && session.panels.includes(ref)) {
      mutate(s => ({ ...s, panels: s.panels.filter(x => x !== ref), openTool: null }));
      return;
    }
    mutate(s => ({
      ...s,
      openTool: ref,
      panels: s.panels.includes(ref) ? s.panels : [...s.panels, ref],
    }));
    requestOpenRail();
  };

  // Dismiss the reaction bar when the user taps outside any open turn.
  // One document listener for all turns because only one bar is open at
  // a time (single-active, mirrors .scrim on sidebar rails in App).
  const openRating = session && session.openRating;
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

  const sendMsg = (val, files) => {
    if (!conv) return;
    // /clear is controller-side — the server rotates the chat file and
    // clears the agent context.  Skip the optimistic user/agent turn
    // append (no LLM reply is coming) and reset local session state
    // once the POST returns.
    if (val.trim() === '/clear' && !(files && files.length)) {
      fetch('/api/conversations/' + encodeURIComponent(conv) + '/turn', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ prompt: '/clear' }),
      }).then(r => {
        if (!r.ok) return;
        mutate(s => ({
          ...s,
          liveTurns: [], artefacts: [], panels: [],
          openTool: null, thinkingRef: null, liveToolRef: null,
        }));
      });
      return;
    }

    const ts = new Date().toTimeString().slice(0, 8);
    const userBlocks = [{ type: 'text', text: val }];
    for (const f of (files || [])) {
      userBlocks.push({
        type: 'file', name: f.name,
        mime: f.type || 'application/octet-stream',
        size: f.size,
        url: f.type && f.type.startsWith('image/') ? URL.createObjectURL(f) : null,
        local: true,
      });
    }
    const userTurn = { role: 'user', ts, blocks: userBlocks };
    const aTurn = { role: 'agent', ts, blocks: [{ type: 'text', text: '' }] };
    mutate(s => ({
      ...s,
      running: true,
      phase: 'thinking',
      thinkingRef: null,
      liveTurns: [...s.liveTurns, userTurn, aTurn],
    }));

    const es = client.send(conv, val, streamCallbacks(conv), files);
    const resources = getResources(conv);
    resources.es = es;
  };

  const onCancel = () => {
    if (conv) client.cancel(conv).catch(() => {});
    const resources = getResources(conv);
    if (resources.es) { try { resources.es.close(); } catch { /* already closed */ } resources.es = null; }
    mutate(s => ({ ...s, running: false }));
  };

  const onRate = (turnIndex, emoji) => {
    if (!conv) return;
    const prev = session && session.ratings[turnIndex];
    mutate(s => {
      const next = { ...s.ratings };
      if (emoji) next[turnIndex] = emoji; else delete next[turnIndex];
      return { ...s, ratings: next };
    });
    client.feedback(conv, turnIndex, emoji).catch(() => {
      mutate(s => {
        const back = { ...s.ratings };
        if (prev) back[turnIndex] = prev; else delete back[turnIndex];
        return { ...s, ratings: back };
      });
    });
  };

  const setOpenRating = (turnIndex) => {
    mutate(s => ({ ...s, openRating: s.openRating === turnIndex ? null : turnIndex }));
  };

  if (!session) {
    // First paint before live-ready lands.  Render the centre shell
    // (context + empty transcript + composer dock) so the user sees
    // the app frame instead of a blank screen while conversations
    // hydrate.  Composer sends no-op since there's no chat yet.
    return (
      <div className="centre">
        <div className="context"><div className="crumbs"/></div>
        <div className="transcript"><div className="inner"><EmptyState/></div></div>
        <div className="composer-dock">
          <div style={{width:'100%',maxWidth:820,display:'flex',flexDirection:'column',alignItems:'stretch'}}>
            <Composer onSend={() => {}} onCancel={() => {}} running={false}/>
          </div>
        </div>
      </div>
    );
  }
  const liveConv = conversations.find(c => c.id === conv);
  const title = (liveConv && liveConv.title) || conv || '';
  const showEmpty = session.liveTurns.length === 0 && !session.running;

  const onExport = () => {
    if (!conv) return;
    client.exportConversation(conv).then(blob => {
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${conv}.sharegpt.json`;
      document.body.appendChild(a); a.click(); a.remove();
      URL.revokeObjectURL(url);
    }).catch(e => {
      console.warn('[dyson] export failed', e);
      alert(`Export failed: ${e.message || e}`);
    });
  };

  return (
    <div className="centre">
      <div className="context">
        <div className="crumbs"><span className="c-leaf">{title}</span></div>
        <div className="right">
          <button className="btn sm ghost" title="Download ShareGPT export"
                  onClick={onExport} disabled={!conv}
                  style={{padding:'4px 8px'}}>
            <Icon name="download" size={13}/>
          </button>
        </div>
      </div>
      <div className="transcript" ref={scrollRef}>
        <div className="inner">
          {showEmpty ? (
            <EmptyState/>
          ) : (
            session.liveTurns.map((t, i) => (
              <Turn key={i} turn={t} tools={tools}
                    onOpenTool={handleOpenTool} activeTool={session.openTool}
                    turnIndex={i} rating={session.ratings[i]} onRate={onRate}
                    reactionsOpen={session.openRating === i}
                    onToggleReactions={() => setOpenRating(i)}/>
            ))
          )}
        </div>
      </div>
      {/* Sibling to .transcript so .transcript's overflow:auto doesn't
          clip the dock and the dock's bottom:0 resolves to the bottom
          of .centre (which is position: relative). */}
      <div className="composer-dock">
        <div style={{width:'100%',maxWidth:820,display:'flex',flexDirection:'column',alignItems:'stretch'}}>
          {session.running && (
            <TypingIndicator phase={session.phase} tname={session.tname}
                             onJump={() => {
                               if (session.liveToolRef) handleOpenTool(session.liveToolRef);
                             }}/>
          )}
          <Composer onSend={sendMsg} onCancel={onCancel} running={session.running}/>
        </div>
      </div>
    </div>
  );
}

// -- SSE callback builder -----------------------------------------------
// Produces the callback bag passed to DysonClient.send for a specific
// chat.  Each callback is a reducer over either the session store (for
// transcript / panels / running flags) or the app store (for tool views
// shared across components).  All state flows through the stores — no
// mutations touch local references here, so `bump()` and the attendant
// "silently dropped render" bug are structurally impossible.
function streamCallbacks(conv) {
  return {
    onText: (delta) => {
      updateSession(conv, s => {
        const turns = s.liveTurns;
        if (!turns.length) return s;
        const last = turns[turns.length - 1];
        const blocks = last.blocks;
        const tail = blocks[blocks.length - 1];
        const newBlocks = (tail && tail.type === 'text')
          ? [...blocks.slice(0, -1), { ...tail, text: tail.text + delta }]
          : [...blocks, { type: 'text', text: delta }];
        return { ...s, liveTurns: [...turns.slice(0, -1), { ...last, blocks: newBlocks }] };
      });
    },
    onThinking: (delta) => {
      const existingRef = (getSession(conv) || {}).thinkingRef;
      if (existingRef) {
        // Append to the existing thinking panel.
        updateTool(existingRef, t => ({ ...t, body: { ...t.body, text: (t.body.text || '') + delta } }));
        return;
      }
      // Mint a new thinking panel for this turn — ONE per turn, reused
      // across deltas via session.thinkingRef.
      const ref = mintToolRef(conv, 'thinking');
      setTool(ref, mkTool('thinking', {
        kind: 'thinking', status: 'running', dur: '…', body: { text: delta },
      }));
      updateSession(conv, s => {
        const turns = s.liveTurns;
        if (!turns.length) return s;
        const last = turns[turns.length - 1];
        const newTurn = { ...last, blocks: [...last.blocks, { type: 'tool', ref }] };
        return {
          ...s,
          thinkingRef: ref,
          openTool: ref,
          panels: s.panels.includes(ref) ? s.panels : [...s.panels, ref],
          liveTurns: [...turns.slice(0, -1), newTurn],
        };
      });
    },
    onToolStart: ({ id, name }) => {
      const ref = id || mintToolRef(conv, 'live');
      setTool(ref, mkTool(name, { status: 'running', dur: '…' }));
      updateSession(conv, s => {
        const turns = s.liveTurns;
        if (!turns.length) return s;
        const last = turns[turns.length - 1];
        const newTurn = { ...last, blocks: [...last.blocks, { type: 'tool', ref }] };
        return {
          ...s,
          phase: 'tool',
          tname: name,
          liveToolRef: ref,
          openTool: ref,
          panels: s.panels.includes(ref) ? s.panels : [...s.panels, ref],
          liveTurns: [...turns.slice(0, -1), newTurn],
        };
      });
    },
    onToolResult: ({ content, is_error, view }) => {
      const ref = (getSession(conv) || {}).liveToolRef;
      if (!ref) return;
      updateTool(ref, t => applyToolView(t, content, is_error, view));
      // image_generate's content is `Generated N image(s) for: "…"` —
      // capture the prompt on a sibling field so ImagePanel can surface
      // it alongside the picture.
      if (typeof content === 'string') {
        const m = content.match(/^Generated \d+ image\(s\) for: "(.+)"$/s);
        if (m) updateTool(ref, t => t.name === 'image_generate' ? { ...t, prompt: m[1] } : t);
      }
    },
    onCheckpoint: ({ text }) => {
      const ref = (getSession(conv) || {}).liveToolRef;
      if (!ref) return;
      updateTool(ref, t => ({ ...t, body: { ...t.body, text: (t.body.text || '') + text + '\n' } }));
    },
    onError: (message) => {
      updateSession(conv, s => {
        const turns = s.liveTurns;
        if (!turns.length) return s;
        const last = turns[turns.length - 1];
        const newTurn = { ...last, blocks: [...last.blocks, { type: 'text', text: `\n[error] ${message}\n` }] };
        return { ...s, liveTurns: [...turns.slice(0, -1), newTurn] };
      });
    },
    onFile: ({ name, mime_type, url, inline_image }) => {
      // Agent-produced files (e.g. image_generate, exploit_builder).
      updateSession(conv, s => {
        const turns = s.liveTurns;
        if (!turns.length) return s;
        const last = turns[turns.length - 1];
        const newTurn = { ...last, blocks: [...last.blocks, {
          type: 'file', name, mime: mime_type, url, inline: !!inline_image,
        }] };
        return { ...s, liveTurns: [...turns.slice(0, -1), newTurn] };
      });
      // For images: also attach the URL to the active tool panel so the
      // right-rail shows the generated image without scrolling chat.
      if (inline_image) {
        const ref = (getSession(conv) || {}).liveToolRef;
        if (ref) {
          updateTool(ref, t => ({
            ...t,
            kind: 'image',
            body: { url, name, mime: mime_type, prompt: t.prompt || '' },
          }));
        }
      }
    },
    onArtefact: ({ id, kind, title, url, bytes, metadata }) => {
      // Image artefacts double up on screen during live turns — the
      // preceding `file` event already rendered a FileBlock with the
      // same <img>.  Skip the transcript chip for images; still track
      // in session.artefacts so the Artefacts tab lists them.
      updateSession(conv, s => {
        let liveTurns = s.liveTurns;
        if (kind !== 'image' && liveTurns.length) {
          const last = liveTurns[liveTurns.length - 1];
          const newTurn = { ...last, blocks: [...last.blocks, { type: 'artefact', id, kind, title, url, bytes }] };
          liveTurns = [...liveTurns.slice(0, -1), newTurn];
        }
        const artefacts = [
          { id, kind, title, bytes, created_at: Math.floor(Date.now() / 1000), metadata },
          ...s.artefacts,
        ];
        return { ...s, liveTurns, artefacts };
      });
      // Mark the conversation as having artefacts so the Artefacts
      // view's filtered sidebar picks it up without waiting for a
      // full /api/conversations refresh.
      markConversationHasArtefacts(conv);
    },
    onDone: () => {
      const ref = (getSession(conv) || {}).thinkingRef;
      if (ref) updateTool(ref, t => ({ ...t, status: 'done', dur: '' }));
      updateSession(conv, s => ({ ...s, running: false, thinkingRef: null }));
      const resources = getResources(conv);
      resources.es = null;
    },
  };
}

// -- Transcript hydration -----------------------------------------------
// Consumes the /api/conversations/:id response and seeds the session
// store + the tool view dict.  Preserves the same retroactive
// tool-use_id pairing the old app.jsx did for legacy chats whose image
// artefacts were emitted before tool_use_id tracking landed.
function hydrateTranscript(conv, data) {
  const toolUpdates = {}; // ref -> Tool
  const getCounter = () => {
    const r = getResources(conv);
    r.counter += 1;
    return r.counter;
  };
  const turns = (data.messages || []).map(m => {
    const role = m.role === 'user' ? 'user' : 'agent';
    const blocks = [];
    for (const b of m.blocks) {
      if (b.type === 'text') blocks.push({ type: 'text', text: b.text });
      else if (b.type === 'thinking') blocks.push({ type: 'thinking', text: b.thinking });
      else if (b.type === 'file') {
        blocks.push({
          type: 'file', name: b.name, mime: b.mime,
          size: b.bytes, url: b.url, inline: !!b.inline_image,
        });
      } else if (b.type === 'tool_use') {
        const id = b.id || `${conv}-tu-${getCounter()}`;
        if (!toolUpdates[id]) toolUpdates[id] = mkTool(b.name);
        if (b.name === 'image_generate' && b.input && typeof b.input.prompt === 'string') {
          toolUpdates[id].prompt = b.input.prompt;
        }
        blocks.push({ type: 'tool', ref: id });
      } else if (b.type === 'tool_result') {
        const t = toolUpdates[b.tool_use_id];
        if (t) { t.status = 'done'; t.exit = b.is_error ? 'err' : 'ok'; t.body = { text: b.content }; }
      } else if (b.type === 'artefact') {
        blocks.push({
          type: 'artefact', id: b.id, kind: b.kind, title: b.title,
          url: b.url, bytes: b.bytes, tool_use_id: b.tool_use_id,
          metadata: b.metadata,
        });
        // Image artefact with a known tool_use_id: flip the matching
        // tool panel into image-kind so the right-rail shows the
        // picture instead of text.
        if (b.kind === 'image' && b.tool_use_id && toolUpdates[b.tool_use_id]) {
          const t = toolUpdates[b.tool_use_id];
          const fileUrl = (b.metadata && b.metadata.file_url) || b.url;
          t.kind = 'image';
          t.body = {
            url: fileUrl,
            name: (b.metadata && b.metadata.file_name) || b.title,
            mime: 'image/*',
            prompt: t.prompt || '',
          };
        }
      }
    }
    return { role, ts: '', blocks };
  });

  // Retroactive wiring: image artefacts emitted BEFORE tool_use_id
  // tracking landed have no correlation to their originating tool.
  // Fall back to a positional heuristic — pair the i-th unassigned
  // image artefact with the i-th `image_generate` tool panel that
  // isn't already showing an image.  This is how old chats get their
  // images back after the user upgrades.
  const orphanImages = [];
  for (const t of turns) {
    for (const b of t.blocks) {
      if (b.type === 'artefact' && b.kind === 'image' && !b.tool_use_id) {
        orphanImages.push(b);
      }
    }
  }
  const pendingFetches = []; // [{ref, artefact}] — raw URL in file body
  if (orphanImages.length > 0) {
    for (const t of turns) {
      for (const b of t.blocks) {
        if (b.type !== 'tool') continue;
        const tool = toolUpdates[b.ref];
        if (!tool || tool.name !== 'image_generate' || tool.kind === 'image') continue;
        const art = orphanImages.shift();
        if (!art) break;
        const metaUrl = art.metadata && art.metadata.file_url;
        const fileName = (art.metadata && art.metadata.file_name) || art.title;
        tool.kind = 'image';
        tool.body = { url: metaUrl || '', name: fileName, mime: 'image/*' };
        if (!metaUrl) pendingFetches.push({ ref: b.ref, art });
      }
    }
  }

  // Commit the tool dict in one dispatch so components re-render once
  // instead of once per tool.
  app.dispatch(s => ({ ...s, tools: { ...s.tools, ...toolUpdates } }));

  // Drop turns that have no renderable blocks — happens when a
  // role='user' message carries only tool_result content.
  const liveTurns = turns.filter(t => t.blocks.length > 0);

  // Seed the Artefacts tab so findArtefactMeta in the reader finds
  // metadata without a second fetch.
  const artefacts = [];
  for (const t of turns) {
    for (const b of t.blocks) {
      if (b.type === 'artefact') {
        artefacts.push({
          id: b.id, kind: b.kind, title: b.title, bytes: b.bytes,
          created_at: 0, metadata: b.metadata,
        });
      }
    }
  }

  updateSession(conv, s => ({ ...s, liveTurns, artefacts }));

  // Resolve orphan-artefact URLs out-of-band (older chats whose artefact
  // body is the raw file URL as a text blob).
  for (const { ref, art } of pendingFetches) {
    fetch(art.url)
      .then(r => r.ok ? r.text() : Promise.reject(r.status))
      .then(text => {
        updateTool(ref, t => ({ ...t, body: { ...t.body, url: text.trim() } }));
      })
      .catch(() => {});
  }
}

function mkTool(name, over = {}) {
  return {
    name,
    icon: (name && name[0] || '?').toUpperCase(),
    sig: '',
    dur: '',
    exit: 'ok',
    status: 'done',
    kind: 'fallback',
    body: { text: '' },
    ...over,
  };
}

// Apply a typed ToolView (from SSE) to a tool entry, producing a new
// frozen value.  Pure — used as a reducer argument to updateTool.
function applyToolView(t, content, isError, view) {
  const base = { ...t, status: 'done', exit: isError ? 'err' : 'ok' };
  if (!view || !view.kind) {
    return { ...base, kind: 'fallback', body: { text: content } };
  }
  const { kind: _k, ...body } = view;
  const next = { ...base, kind: view.kind, body };
  if (view.kind === 'bash' && typeof view.duration_ms === 'number') {
    next.dur = view.duration_ms < 1000
      ? view.duration_ms + 'ms'
      : (view.duration_ms / 1000).toFixed(1) + 's';
  }
  if (view.kind === 'diff' && view.files && view.files[0]) {
    next.sig = view.files[0].path;
    next.meta = `+${view.files[0].add} −${view.files[0].rem}`;
  }
  if (view.kind === 'read' && view.path) next.sig = view.path;
  return next;
}

export { App };
