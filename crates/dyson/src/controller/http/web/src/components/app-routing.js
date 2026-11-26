/* App-level routing helpers: the view list, hash <-> state mapping and
 * the mobile keyboard inset.  Pure functions, unit-tested directly. */

// Single source of truth for the views that exist.  TopBar's nav and
// the ⌘1..N keyboard handler both key off this list so adding/removing
// a view is a one-place change.
export const VIEW_IDS = ['conv', 'mind', 'artefacts', 'activity', 'audit'];

export function keyboardInsetForVisualViewport({
  editing,
  innerHeight,
  visualViewportHeight,
  visualViewportOffsetTop = 0,
  visualViewportScale = 1,
  appHeight = 0,
}) {
  if (!editing) return 0;
  if (Math.abs((visualViewportScale || 1) - 1) > 0.01) return 0;
  const raw = Math.max(0, (innerHeight || 0) - (visualViewportHeight || 0) - (visualViewportOffsetTop || 0));
  if (!raw) return 0;

  // Modern WebKit can resize 100dvh when the software keyboard opens.
  // In that case .app is already the visual viewport height; adding the
  // raw visualViewport delta again pushes the composer halfway up the
  // screen, which reads as the page zooming. Older iOS leaves .app at
  // the layout viewport height, so those builds still need the offset.
  if (appHeight > 0 && appHeight <= (visualViewportHeight || 0) + 8) return 0;
  return raw;
}

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
