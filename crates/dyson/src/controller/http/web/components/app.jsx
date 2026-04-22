/* Dyson — app root.  Live mode only; no seed-data branches.
 *
 * Per-chat state lives in a single `sessionsRef` Map keyed by chat_id.
 * Switching `conv` does NOT clear the prior chat's transcript, panels,
 * ratings, running flag, or in-flight EventSource — they all stay in
 * the map and surface again when the user switches back.  This is the
 * fix for "moving from a chat seems to kill it" and "the tool stack is
 * not per conversation".  See controller/http/mod.rs `#[cfg(test)]
 * mod tests` for the regression checks that lock this behaviour in.
 */

const { useState, useEffect, useRef, useCallback } = React;

// Single source of truth for the views that exist.  TopBar's nav array
// must list these in the same order; the keyboard handler in App keys
// off this list so adding/removing a view requires updating only one
// place.  See tests/http_controller.rs for the regression check.
const VIEW_IDS = ['conv', 'mind', 'artefacts', 'activity'];

// Map chat_id → per-chat session.  Held outside React state because
// each Session contains live mutable refs (an EventSource, a counter,
// a streaming assistant turn) and cloning them on every state update
// would either break SSE or cost us O(transcript) per delta.  We bump
// a dedicated counter to force re-renders.
function makeSession() {
  return {
    liveTurns: [],
    ratings: {},
    panels: [],
    openTool: null,
    // turnIndex whose reaction bar is currently open via explicit tap;
    // null = none.  Mirrors openTool's single-active pattern.
    openRating: null,
    running: false,
    phase: 'thinking',
    tname: '',
    liveToolRef: null,
    es: null,
    counter: 0,
    loaded: false,
    justScrollOnNextRender: false,
    // Per-chat list of artefacts produced by the agent.  Populated from
    // SSE `artefact` events (live) and from `/api/conversations/:id/artefacts`
    // on chat reload.
    artefacts: [],
    artefactsLoaded: false,
  };
}

function App() {
  const [view, setView] = useState('conv');
  const [conv, setConv] = useState(null);
  const [showLeft, setShowLeft] = useState(false);
  const [showRight, setShowRight] = useState(false);
  // Desktop-only: collapse the right column entirely.
  const [rightHidden, setRightHidden] = useState(false);
  // Single force-rerender counter shared by every session (cheap because
  // React batches, and only re-renders the active view).
  const [, force] = useState(0);
  const bump = useCallback(() => force(n => n + 1), []);

  // chat_id → Session.  Created lazily on first access so unopened
  // chats cost nothing.  Exposed on window so the Artefacts reader can
  // pull cached metadata without prop-drilling — see findArtefactMeta
  // in views.jsx.
  const sessionsRef = useRef(new Map());
  useEffect(() => {
    window.__dysonSessions = sessionsRef.current;
    return () => { if (window.__dysonSessions === sessionsRef.current) delete window.__dysonSessions; };
  }, []);
  const getSession = useCallback((id) => {
    if (!id) return null;
    let s = sessionsRef.current.get(id);
    if (!s) { s = makeSession(); sessionsRef.current.set(id, s); }
    return s;
  }, []);

  // Re-render on bridge updates (conversations, providers, mind, skills).
  useEffect(() => {
    const h = () => {
      bump();
      if (!conv) {
        const first = (window.DYSON_DATA.conversations.http || [])[0];
        if (first) setConv(first.id);
      }
    };
    window.addEventListener('dyson:live-ready', h);
    window.addEventListener('dyson:live-update', h);
    return () => {
      window.removeEventListener('dyson:live-ready', h);
      window.removeEventListener('dyson:live-update', h);
    };
  }, [conv, bump]);

  // In-chat artefact chip → flip to the Artefacts tab.  The chip fires
  // `dyson:open-artefact` with the id; ArtefactsView picks the same
  // event up to set the selected artefact.
  useEffect(() => {
    const h = () => setView('artefacts');
    window.addEventListener('dyson:open-artefact', h);
    return () => window.removeEventListener('dyson:open-artefact', h);
  }, []);

  // ⌘1..N view switching (bounds-checked against VIEW_IDS — pressing
  // ⌘4/⌘5 used to point at the deleted Providers/Sandbox views and
  // grey-screen the app), ⌘N for new conversation.
  useEffect(() => {
    const h = (e) => {
      if (!(e.metaKey || e.ctrlKey)) return;
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        if (idx < VIEW_IDS.length) {
          e.preventDefault();
          setView(VIEW_IDS[idx]);
        }
      } else if (e.key === 'n' && window.DysonLive) {
        e.preventDefault();
        window.DysonLive.createChat('New conversation').then(c => {
          window.DYSON_DATA.conversations.http.unshift({ id: c.id, title: c.title, live: false });
          setConv(c.id);
          bump();
        });
      }
    };
    window.addEventListener('keydown', h);
    return () => window.removeEventListener('keydown', h);
  }, [bump]);

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

  // ConversationView dispatches `dyson:open-rail` when a tool chip is
  // clicked.  Force the rail open in whichever mode applies.
  useEffect(() => {
    const h = () => {
      if (window.matchMedia('(max-width: 760px)').matches) {
        setShowRight(true); setShowLeft(false);
      } else {
        setRightHidden(false);
      }
    };
    window.addEventListener('dyson:open-rail', h);
    return () => window.removeEventListener('dyson:open-rail', h);
  }, []);

  const bodyClass = `body${showLeft ? ' show-left' : ''}${showRight ? ' show-right' : ''}${rightHidden ? ' no-right' : ''}`;
  const session = getSession(conv);
  const removePanel = useCallback((ref) => {
    if (!session) return;
    session.panels = session.panels.filter(x => x !== ref);
    bump();
  }, [session, bump]);

  return (
    <div className="app">
      <TopBar view={view} setView={setView}
              rightHidden={rightHidden}
              onToggleLeft={() => { setShowLeft(s => !s); setShowRight(false); }}
              onToggleRight={onToggleRight}/>
      {view === 'conv' && (
        <div className={bodyClass}>
          {(showLeft || showRight) && <div className="scrim" onClick={closeRails}/>}
          <LeftRail active={conv} setActive={(id) => { setConv(id); setShowLeft(false); }}/>
          <ConversationView conv={conv} session={session} bump={bump}/>
          {!rightHidden && <RightRail panels={session ? session.panels : []} onClose={removePanel}/>}
        </div>
      )}
      {view === 'mind' && (
        <div className="body no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <LeftRailMind/>
          <MindView showSide={showLeft} onHideSide={() => setShowLeft(false)}/>
        </div>
      )}
      {view === 'artefacts' && (
        <div className="body no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <LeftRail active={conv} setActive={(id) => { setConv(id); setShowLeft(false); }}/>
          <ArtefactsView conv={conv} session={session} bump={bump}/>
        </div>
      )}
      {view === 'activity' && <div style={{display:'flex', flex:1, minHeight:0}}><ActivityView/></div>}
    </div>
  );
}

// Per-chat state lives in `session` (owned by App).  This component is
// a renderer over the active session; switching `conv` swaps which
// session it reads from but does NOT mutate any session it leaves.
// Streaming SSE for inactive chats keeps populating their session, so
// switching back picks up exactly where we left off.
function ConversationView({ conv, session, bump }) {
  const D = window.DYSON_DATA;
  const scrollRef = useRef(null);

  // First time this conv is opened in this app instance, hydrate from
  // the API (transcript + ratings).  Subsequent switches do nothing
  // because session.loaded is already true.
  useEffect(() => {
    if (!conv || !session || !window.DysonLive) return;
    session.justScrollOnNextRender = true;
    if (session.loaded) { bump(); return; }
    session.loaded = true;
    const ratingToEmoji = {
      terrible: '💩', bad: '👎', not_good: '😐',
      good: '👍', very_good: '🔥', excellent: '❤️',
    };
    window.DysonLive.loadFeedback(conv).then(entries => {
      const map = {};
      for (const e of entries || []) map[e.turn_index] = ratingToEmoji[e.rating] || '';
      session.ratings = map;
      bump();
    }).catch(() => {});
    window.DysonLive.load(conv).then(data => {
      const turns = (data.messages || []).map(m => {
        const role = m.role === 'user' ? 'user' : 'agent';
        const blocks = [];
        for (const b of m.blocks) {
          if (b.type === 'text') blocks.push({ type: 'text', text: b.text });
          else if (b.type === 'thinking') blocks.push({ type: 'thinking', text: b.thinking });
          else if (b.type === 'tool_use') {
            // Namespace tool ids by chat_id — D.tools is global so two
            // chats minting `live-1` would otherwise collide.
            const id = b.id || `${conv}-tu-${++session.counter}`;
            if (!D.tools[id]) D.tools[id] = mkTool(b.name);
            blocks.push({ type: 'tool', ref: id });
          } else if (b.type === 'tool_result') {
            const t = D.tools[b.tool_use_id];
            if (t) { t.status = 'done'; t.exit = b.is_error ? 'err' : 'ok'; t.body = { text: b.content }; }
          } else if (b.type === 'artefact') {
            // Server synthesises a trailing assistant turn from the
            // ArtefactStore on chat reload — render those as chips so
            // images / reports stay visible across refreshes.
            blocks.push({
              type: 'artefact',
              id: b.id,
              kind: b.kind,
              title: b.title,
              url: b.url,
              bytes: b.bytes,
            });
          }
        }
        return { role, ts: '', blocks };
      });
      session.liveTurns = turns;
      // Seed the Artefacts tab so findArtefactMeta in the reader finds
      // the metadata without a second fetch.
      session.artefacts = [];
      for (const t of turns) {
        for (const b of t.blocks) {
          if (b.type === 'artefact') {
            session.artefacts.push({
              id: b.id,
              kind: b.kind,
              title: b.title,
              bytes: b.bytes,
              created_at: 0,
            });
          }
        }
      }
      bump();
    }).catch(() => {});
  }, [conv, session, bump]);

  // Auto-scroll: force-bottom the first time we render with content
  // for this conv (handles "open at bottom"), then "near-bottom only"
  // for subsequent streaming deltas (don't yank a user reading older
  // context).  Re-runs on every bump so streaming follows.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !session) return;
    if (session.justScrollOnNextRender && session.liveTurns.length > 0) {
      el.scrollTop = el.scrollHeight;
      session.justScrollOnNextRender = false;
      return;
    }
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 240;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  });

  const ensureRailOpen = () => window.dispatchEvent(new CustomEvent('dyson:open-rail'));

  const handleOpenTool = (ref) => {
    session.openTool = ref;
    if (!session.panels.includes(ref)) session.panels = [...session.panels, ref];
    ensureRailOpen();
    bump();
  };

  const sendMsg = (val, files) => {
    if (!conv || !session || !window.DysonLive) return;
    session.running = true;
    session.phase = 'thinking';
    const ts = new Date().toTimeString().slice(0, 8);
    // Show attachments alongside the user's text in the optimistic
    // user turn so the UI reflects what was sent before the agent
    // replies.  Each File becomes a `file` block; bridge handles the
    // base64 encoding for the actual transport.
    const userBlocks = [{ type: 'text', text: val }];
    for (const f of (files || [])) {
      userBlocks.push({
        type: 'file', name: f.name,
        mime: f.type || 'application/octet-stream',
        size: f.size,
        // Object URL for inline preview of images we just sent;
        // revoked when the chat tab is closed (browser GCs it).
        url: f.type && f.type.startsWith('image/') ? URL.createObjectURL(f) : null,
        local: true,
      });
    }
    const userTurn = { role: 'user', ts, blocks: userBlocks };
    const aTurn = { role: 'agent', ts, blocks: [{ type: 'text', text: '' }] };
    session.liveTurns = [...session.liveTurns, userTurn, aTurn];
    session.thinkingRef = null; // one thinking panel per turn
    let activeText = aTurn.blocks[0];
    bump();

    session.es = window.DysonLive.send(conv, val, {
      onText: (delta) => {
        if (!activeText || activeText.type !== 'text') {
          activeText = { type: 'text', text: '' };
          aTurn.blocks.push(activeText);
        }
        activeText.text += delta;
        bump();
      },
      onThinking: (delta) => {
        // Live-stream the model's extended-thinking into a right-rail
        // panel.  The panel is a synthetic "tool" with kind='thinking'
        // so ToolPanel renders it through the existing pipeline.  We
        // reuse one ref per turn (keyed off the turn's start counter)
        // so all deltas land in the same panel.
        const ref = session.thinkingRef || `${conv}-thinking-${++session.counter}`;
        session.thinkingRef = ref;
        if (!D.tools[ref]) {
          D.tools[ref] = mkTool('thinking', {
            kind: 'thinking',
            status: 'running',
            dur: '…',
            body: { text: '' },
          });
          aTurn.blocks.push({ type: 'tool', ref });
          if (!session.panels.includes(ref)) session.panels = [...session.panels, ref];
          session.openTool = ref;
        }
        D.tools[ref].body.text = (D.tools[ref].body.text || '') + delta;
        bump();
      },
      onToolStart: ({ id, name }) => {
        session.phase = 'tool';
        session.tname = name;
        const ref = id || `${conv}-live-${++session.counter}`;
        D.tools[ref] = mkTool(name, { status: 'running', dur: '…' });
        aTurn.blocks.push({ type: 'tool', ref });
        if (!session.panels.includes(ref)) session.panels = [...session.panels, ref];
        session.openTool = ref;
        session.liveToolRef = ref;
        activeText = null;
        bump();
      },
      onToolResult: ({ content, is_error, view }) => {
        const ref = session.liveToolRef;
        const t = ref && D.tools[ref];
        if (t) applyToolView(t, content, is_error, view);
        session.liveToolRef = null;
        bump();
      },
      onCheckpoint: ({ text }) => {
        const t = session.liveToolRef && D.tools[session.liveToolRef];
        if (t) { t.body.text = (t.body.text || '') + text + '\n'; bump(); }
      },
      onError: (message) => {
        aTurn.blocks.push({ type: 'text', text: `\n[error] ${message}\n` });
        bump();
      },
      onFile: ({ name, mime_type, url, inline_image }) => {
        // Agent-produced files (e.g. image_generate, exploit_builder).
        // Renders inline as <img> for images, otherwise a download link.
        aTurn.blocks.push({ type: 'file', name, mime: mime_type, url, inline: !!inline_image });
        // For images: also attach the URL to the active tool panel so
        // the right-rail's ToolPanel shows the generated image without
        // the user having to scroll chat.  Targets whichever tool
        // call is currently live (typically `image_generate`).
        if (inline_image) {
          const ref = session.liveToolRef;
          const t = ref && D.tools[ref];
          if (t) {
            t.kind = 'image';
            t.body = { url, name, mime: mime_type };
          }
        }
        bump();
      },
      onArtefact: ({ id, kind, title, url, bytes, metadata }) => {
        // Inline chip in chat + add to the per-chat artefacts list so
        // the Artefacts tab lists it without a second fetch.
        aTurn.blocks.push({ type: 'artefact', id, kind, title, url, bytes });
        session.artefacts = [
          { id, kind, title, bytes, created_at: Math.floor(Date.now() / 1000), metadata },
          ...session.artefacts,
        ];
        bump();
      },
      onDone: () => {
        // Mark the thinking panel (if any) as complete so it stops
        // showing the "running" pulse.
        if (session.thinkingRef && D.tools[session.thinkingRef]) {
          D.tools[session.thinkingRef].status = 'done';
          D.tools[session.thinkingRef].dur = '';
        }
        session.running = false;
        session.es = null;
        bump();
      },
    }, files);
  };

  const onCancel = () => {
    if (!session) return;
    if (conv && window.DysonLive) window.DysonLive.cancel(conv).catch(() => {});
    if (session.es) { session.es.close(); session.es = null; }
    session.running = false;
    bump();
  };

  const onRate = (turnIndex, emoji) => {
    if (!conv || !session || !window.DysonLive) return;
    const prev = session.ratings[turnIndex];
    const next = { ...session.ratings };
    if (emoji) next[turnIndex] = emoji; else delete next[turnIndex];
    session.ratings = next;
    bump();
    window.DysonLive.feedback(conv, turnIndex, emoji).catch(() => {
      const back = { ...session.ratings };
      if (prev) back[turnIndex] = prev; else delete back[turnIndex];
      session.ratings = back;
      bump();
    });
  };

  const setOpenRating = (turnIndex) => {
    if (!session) return;
    session.openRating = session.openRating === turnIndex ? null : turnIndex;
    bump();
  };

  // Dismiss the reaction bar when the user taps outside any open turn.
  // One document listener for all turns because only one bar is open at
  // a time (single-active, mirrors .scrim on sidebar rails in App).
  const openRating = session && session.openRating;
  useEffect(() => {
    if (!session || openRating == null) return;
    const h = (e) => {
      if (!e.target.closest('.turn.reactions-open')) {
        session.openRating = null;
        bump();
      }
    };
    document.addEventListener('pointerdown', h);
    return () => document.removeEventListener('pointerdown', h);
  }, [session, openRating, bump]);

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
  const liveConv = (D.conversations.http || []).find(c => c.id === conv);
  const title = (liveConv && liveConv.title) || conv || '';
  const showEmpty = session.liveTurns.length === 0 && !session.running;

  return (
    <div className="centre">
      <div className="context">
        <div className="crumbs"><span className="c-leaf">{title}</span></div>
      </div>
      <div className="transcript" ref={scrollRef}>
        <div className="inner">
          {showEmpty ? (
            <EmptyState/>
          ) : (
            session.liveTurns.map((t, i) => (
              <Turn key={i} turn={t} tools={D.tools}
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
                             onJump={() => session.liveToolRef && handleOpenTool(session.liveToolRef)}/>
          )}
          <Composer onSend={sendMsg} onCancel={onCancel} running={session.running}/>
        </div>
      </div>
    </div>
  );
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

// Apply a typed ToolView (from SSE) to a tool entry, in-place.
function applyToolView(t, content, isError, view) {
  t.status = 'done';
  t.exit = isError ? 'err' : 'ok';
  if (!view || !view.kind) {
    t.kind = 'fallback';
    t.body = { text: content };
    return;
  }
  t.kind = view.kind;
  const { kind: _k, ...body } = view;
  t.body = body;
  if (view.kind === 'bash' && typeof view.duration_ms === 'number') {
    t.dur = view.duration_ms < 1000
      ? view.duration_ms + 'ms'
      : (view.duration_ms / 1000).toFixed(1) + 's';
  }
  if (view.kind === 'diff' && view.files && view.files[0]) {
    t.sig = view.files[0].path;
    t.meta = `+${view.files[0].add} −${view.files[0].rem}`;
  }
  if (view.kind === 'read' && view.path) t.sig = view.path;
}

function LeftRailMind() {
  const files = (window.DYSON_DATA && window.DYSON_DATA.mind && window.DYSON_DATA.mind.files) || [];
  const journal = files.filter(f => f.path && f.path.startsWith('memory/')).length;
  return (
    <aside className="left">
      <div style={{padding:'14px'}}>
        <div className="eyebrow" style={{marginBottom:8}}>WORKSPACE</div>
        <div style={{display:'flex', flexDirection:'column', gap:6, fontSize:12, color:'var(--fg-dim)'}}>
          <div style={{display:'flex', justifyContent:'space-between'}}><span>Files</span><span className="mono">{files.length}</span></div>
          <div style={{display:'flex', justifyContent:'space-between'}}><span>Journal entries</span><span className="mono">{journal}</span></div>
        </div>
      </div>
    </aside>
  );
}

const root = ReactDOM.createRoot(document.getElementById('root'));
root.render(<App/>);
