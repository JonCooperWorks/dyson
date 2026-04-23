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

const { useState, useEffect, useRef, useCallback, useLayoutEffect } = React;

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
  const initialRoute = parseHash();
  // Prime the deep-link stash on first render so ArtefactsView's mount
  // effect picks the right id.  We clear it in the hashchange handler
  // below whenever the hash moves away from `#/artefacts/<id>`.
  if (initialRoute.artefactId) {
    window.__dysonOpenArtefactId = initialRoute.artefactId;
  }
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
  }, []);
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
      // Deep-link into a specific artefact.  We stash the id on window
      // so ArtefactsView's mount effect picks it up synchronously —
      // React state propagation through context would land one frame
      // late and the reader would briefly show the empty state.
      if (r.artefactId) {
        window.__dysonOpenArtefactId = r.artefactId;
        window.dispatchEvent(new CustomEvent('dyson:open-artefact', { detail: { id: r.artefactId } }));
      }
    };
    window.addEventListener('popstate', h);
    window.addEventListener('hashchange', h);
    return () => {
      window.removeEventListener('popstate', h);
      window.removeEventListener('hashchange', h);
    };
  }, []);

  // In-chat artefact chip → flip to the Artefacts tab.  The chip
  // fires `dyson:open-artefact` with the id; we also stash it on a
  // global so ArtefactsView's initial mount can pick it up even
  // though its own listener isn't attached yet (the tab switch and
  // the listener-attach happen in the same microtask, racing
  // against the event dispatch).
  useEffect(() => {
    const h = (e) => {
      const id = e.detail && e.detail.id;
      if (id) {
        window.__dysonOpenArtefactId = id;
        setArtefactId(id);
      }
      setView('artefacts');
    };
    window.addEventListener('dyson:open-artefact', h);
    return () => window.removeEventListener('dyson:open-artefact', h);
  }, []);

  // Cold deep-link restore: the Artefact reader learns the owning
  // chat_id from the fetch response header and fires this event so
  // the sidebar can hydrate.  No-op when we already have a conv (the
  // in-chat click path).
  useEffect(() => {
    const h = (e) => {
      const id = e.detail && e.detail.id;
      if (id && id !== conv) setConv(id);
    };
    window.addEventListener('dyson:set-conv', h);
    return () => window.removeEventListener('dyson:set-conv', h);
  }, [conv]);

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
      } else if (e.key === 'k' && window.DysonLive) {
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
  }, [bump, selectView]);

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
      <TopBar view={view} setView={selectView}
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
        <div className="body no-left no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <MindView showSide={showLeft} onHideSide={() => setShowLeft(false)}/>
        </div>
      )}
      {view === 'artefacts' && (
        <div className="body no-right">
          {showLeft && <div className="scrim" onClick={closeRails}/>}
          <LeftRail active={conv}
                    setActive={(id) => {
                      // Switching a chat in the Artefacts sidebar should
                      // land on something readable — clear the current
                      // selection so ArtefactsView's hydrate effect
                      // picks the first artefact of the new chat and
                      // updates the URL to match.
                      setConv(id);
                      setArtefactId(null);
                      window.__dysonOpenArtefactId = null;
                      setShowLeft(false);
                    }}
                    filter={(c) => c.hasArtefacts}
                    emptyLabel="No chats with artefacts yet. Run a /security-review in any conversation to create one."/>
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
          else if (b.type === 'file') {
            // User-uploaded image/document restored from disk on chat
            // reload.  FileBlock already knows this shape — the server
            // emits a data URL so there's no extra round-trip.
            blocks.push({
              type: 'file',
              name: b.name,
              mime: b.mime,
              size: b.bytes,
              url: b.url,
              inline: !!b.inline_image,
            });
          }
          else if (b.type === 'tool_use') {
            // Namespace tool ids by chat_id — D.tools is global so two
            // chats minting `live-1` would otherwise collide.
            const id = b.id || `${conv}-tu-${++session.counter}`;
            if (!D.tools[id]) D.tools[id] = mkTool(b.name);
            // Capture image_generate's prompt so ImagePanel can show
            // it alongside the picture.  Kept off `sig` intentionally —
            // the transcript chip stays clean; the prompt lives in the
            // right-rail panel only.
            if (b.name === 'image_generate' && b.input && typeof b.input.prompt === 'string') {
              D.tools[id].prompt = b.input.prompt;
            }
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
              tool_use_id: b.tool_use_id,
              metadata: b.metadata,
            });
            // Image artefact with a known tool_use_id: flip the
            // matching tool panel into image-kind so the right-rail
            // shows the picture instead of text.  This is the refresh
            // path's counterpart to `onFile` during live streaming.
            if (b.kind === 'image' && b.tool_use_id && D.tools[b.tool_use_id]) {
              const t = D.tools[b.tool_use_id];
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
      // Drop turns that have no renderable blocks — happens when a
      // role='user' message carries only tool_result content (the
      // loader folds that into the tool chip state rather than pushing
      // a block), which would otherwise show up as a named-but-empty
      // user avatar in the transcript.
      session.liveTurns = turns.filter(t => t.blocks.length > 0);
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
              metadata: b.metadata,
            });
          }
        }
      }

      // Retroactive wiring: image artefacts emitted BEFORE tool_use_id
      // tracking landed have no correlation to their originating tool.
      // Fall back to a positional heuristic — pair the i-th unassigned
      // image artefact with the i-th `image_generate` tool panel that
      // isn't already showing an image.  This is how old chats get
      // their images back after the user upgrades.
      const orphanImages = [];
      for (const t of turns) {
        for (const b of t.blocks) {
          if (b.type === 'artefact' && b.kind === 'image' && !b.tool_use_id) {
            orphanImages.push(b);
          }
        }
      }
      if (orphanImages.length > 0) {
        for (const t of turns) {
          for (const b of t.blocks) {
            if (b.type !== 'tool') continue;
            const tool = D.tools[b.ref];
            if (!tool || tool.name !== 'image_generate' || tool.kind === 'image') continue;
            const art = orphanImages.shift();
            if (!art) break;
            // Image-artefact body is the served file URL (stored
            // verbatim in `/api/artefacts/<id>`).  Prefer metadata
            // when present (newer emissions) — otherwise fetch the
            // body to resolve the URL.
            const metaUrl = art.metadata && art.metadata.file_url;
            const fileName = (art.metadata && art.metadata.file_name) || art.title;
            tool.kind = 'image';
            tool.body = { url: metaUrl || '', name: fileName, mime: 'image/*' };
            if (!metaUrl) {
              fetch(art.url)
                .then(r => r.ok ? r.text() : Promise.reject(r.status))
                .then(text => {
                  tool.body = { ...tool.body, url: text.trim() };
                  bump();
                })
                .catch(() => {});
            }
          }
        }
      }
      bump();
    }).catch(() => {});
  }, [conv, session, bump]);

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
      session.justScrollOnNextRender = false;
      return;
    }
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 240;
    if (nearBottom) el.scrollTop = el.scrollHeight;
  });

  const ensureRailOpen = () => window.dispatchEvent(new CustomEvent('dyson:open-rail'));

  const handleOpenTool = (ref) => {
    // Toggle: clicking an already-open tool chip closes its panel.
    // Matches the user expectation that the chip is a switch, not a
    // single-shot opener — the right-rail can get crowded fast and
    // click-to-close is the natural way to dismiss.
    if (session.openTool === ref && session.panels.includes(ref)) {
      session.panels = session.panels.filter(x => x !== ref);
      session.openTool = null;
      bump();
      return;
    }
    session.openTool = ref;
    if (!session.panels.includes(ref)) session.panels = [...session.panels, ref];
    ensureRailOpen();
    bump();
  };

  const sendMsg = (val, files) => {
    if (!conv || !session || !window.DysonLive) return;
    // /clear is controller-side — the server rotates the chat file and
    // clears the agent context.  Skip the optimistic user/agent turn
    // append (there's no LLM reply coming) and reset the local
    // transcript once the POST returns.  Without this the sidebar and
    // the chat scroll would still show the pre-clear turns even though
    // disk and agent state have been rotated.
    if (val.trim() === '/clear' && !(files && files.length)) {
      fetch('/api/conversations/' + encodeURIComponent(conv) + '/turn', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ prompt: '/clear' }),
      }).then(r => {
        if (!r.ok) return;
        session.liveTurns = [];
        session.artefacts = [];
        session.panels = [];
        session.openTool = null;
        session.thinkingRef = null;
        session.liveToolRef = null;
        bump();
      });
      return;
    }
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
        // image_generate's content is `Generated N image(s) for: "…"`
        // (prompt truncated to 100 chars server-side).  The file event
        // fires after this one and overwrites body with url/name/mime,
        // so capture the prompt on a sibling field first — ImagePanel
        // reads it back off `tool.prompt`.
        if (t && t.name === 'image_generate' && typeof content === 'string') {
          const m = content.match(/^Generated \d+ image\(s\) for: "(.+)"$/s);
          if (m) t.prompt = m[1];
        }
        // Keep `liveToolRef` pointing at the completed tool so any
        // follow-up `file` / `artefact` / `checkpoint` events that
        // belong to the same tool call can find it.  `onToolStart`
        // replaces it when the next tool runs, and `onDone` is the
        // natural end-of-turn cleanup — we don't need to null it
        // here, and nulling broke image_generate's inline preview
        // (file events arrive after tool_result per execution.rs).
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
            t.body = { url, name, mime: mime_type, prompt: t.prompt || '' };
          }
        }
        bump();
      },
      onArtefact: ({ id, kind, title, url, bytes, metadata }) => {
        // Image artefacts double up on screen during live turns — the
        // preceding `file` event already rendered a FileBlock with the
        // same <img>.  Skip the transcript chip for images but still
        // track them in session.artefacts so the Artefacts tab lists
        // them.  On reload the server only sends artefact blocks (no
        // `file` events to replay), so ArtefactBlock renders there.
        if (kind !== 'image') {
          aTurn.blocks.push({ type: 'artefact', id, kind, title, url, bytes });
        }
        session.artefacts = [
          { id, kind, title, bytes, created_at: Math.floor(Date.now() / 1000), metadata },
          ...session.artefacts,
        ];
        // Mark the conversation as having artefacts so the Artefacts
        // view's filtered sidebar picks it up without waiting for a
        // full /api/conversations refresh.
        const convRow = (D.conversations.http || []).find(c => c.id === conv);
        if (convRow) convRow.hasArtefacts = true;
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

  const onExport = () => {
    if (!conv || !window.DysonLive) return;
    window.DysonLive.exportConversation(conv).catch(e => {
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

const root = ReactDOM.createRoot(document.getElementById('root'));
root.render(<App/>);
