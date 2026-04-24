// Regression tests ported from crates/dyson/src/controller/http/mod.rs.
//
// These used to live on the Rust side and grep embedded JSX source.  That
// coupling meant a frontend refactor required touching mod.rs, and the
// tests spoke a language one step removed from the code they verified.
// Now they live next to the frontend and run under vitest via `npm test`.
//
// Most assertions remain source-text greps (the original checks encoded
// specific past bugs by looking for their patterns).  The markdown tests
// are upgraded to real function calls since the fix is observable in the
// output, not just the source.

import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import * as views from '../components/views.jsx';
import * as viewsSecondary from '../components/views-secondary.jsx';
import * as turns from '../components/turns.jsx';
import * as panels from '../components/panels.jsx';
import * as icons from '../components/icons.jsx';

const __dirname = dirname(fileURLToPath(import.meta.url));
const src = (rel) => readFileSync(join(__dirname, '..', rel), 'utf8');

describe('module exports', () => {
  it('views.jsx exports the primary shell components', () => {
    for (const name of ['TopBar', 'LeftRail', 'RightRail']) {
      expect(views[name], `views.jsx must export ${name}`).toBeTypeOf('function');
    }
  });

  it('views-secondary.jsx exports the lazy-loaded views', () => {
    // These were split out of views.jsx so the cold-load bundle only
    // carries the conversation shell; app.jsx React.lazy()s them.
    for (const name of ['MindView', 'ActivityView', 'ArtefactsView', 'ArtefactReader']) {
      expect(viewsSecondary[name], `views-secondary.jsx must export ${name}`).toBeTypeOf('function');
    }
  });

  it('turns.jsx exports only live components', () => {
    // SubagentCard and ErrorCard were deleted; they must not re-appear.
    for (const dead of ['SubagentCard', 'ErrorCard']) {
      expect(turns[dead], `turns.jsx still exports deleted name ${dead}`).toBeUndefined();
    }
    for (const live of ['Turn', 'ToolChip', 'Composer', 'EmptyState', 'markdown']) {
      expect(turns[live], `turns.jsx must export ${live}`).toBeDefined();
    }
  });

  it('panels.jsx exports the tool panels', () => {
    for (const name of ['PanelChrome', 'BashPanel', 'DiffPanel', 'SbomPanel', 'TaintPanel', 'ThinkingPanel', 'ImagePanel', 'FallbackPanel', 'ReadPanel', 'ToolPanel', 'copyTextForTool']) {
      expect(panels[name], `panels.jsx must export ${name}`).toBeDefined();
    }
  });

  it('icons.jsx exports Icon, Chip, Kbd', () => {
    for (const name of ['Icon', 'Chip', 'Kbd']) {
      expect(icons[name], `icons.jsx must export ${name}`).toBeTypeOf('function');
    }
  });
});

describe('app.jsx — keyboard + session regressions', () => {
  const app = src('components/app.jsx');

  it('keyboard handler does not outrun view ids', () => {
    // Regression for the "⌘4/⌘5 grey-screen" bug: the keyboard handler
    // used to map [1-5] to a hardcoded array longer than the rendered
    // <Route>s.  app.jsx uses VIEW_IDS as the single source of truth
    // with a bounds check.
    expect(app).toContain('const VIEW_IDS');
    expect(app).toContain('idx < VIEW_IDS.length');
    expect(app, 'app.jsx still references deleted Providers/Sandbox views')
      .not.toContain("['conv','mind','activity','providers','sandbox']");
  });

  it('transcript force-scrolls to bottom on load', () => {
    // Regression for "chats open at the top".
    expect(app).toContain('justScrollOnNextRender');
  });

  it('per-chat session state survives conv switch', () => {
    // Regression for "moving from a chat seems to kill it" and "the tool
    // stack is not per conversation".  Sessions now live in a reactive
    // store (src/store/sessions.js) keyed by chatId; ConversationView
    // reads the active session via useSession() and mutates via
    // updateSession() — switching `conv` swaps which slice it reads but
    // does NOT touch any other session.
    expect(app).toContain("useSession(conv)");
    expect(app).toContain("ensureSession");
    expect(app, 'conv-change must NOT wipe liveTurns').not.toContain('setLiveTurns([])');
    expect(
      app.includes('session.panels') || app.includes('session ? session.panels'),
      'RightRail must take its panels from the active session',
    ).toBe(true);
    // EventSource + per-chat counter live in the non-reactive resources
    // map — they're resources, not data, and can't be frozen.
    expect(app).toContain('getResources(conv)');
  });

  it('live tool ids are namespaced per chat', () => {
    // Two chats minting live-1 would collide in app.tools.  Refs flow
    // through mintToolRef(chatId, kind) which prefixes with the chatId.
    const sessionsSrc = src('store/sessions.js');
    expect(sessionsSrc).toContain('mintToolRef');
    expect(sessionsSrc).toContain('`${chatId}-${kind}-${r.counter}`');
    // ConversationView calls it on every onToolStart / onThinking.
    expect(app).toContain("mintToolRef(conv, 'live')");
    expect(app).toContain("mintToolRef(conv, 'thinking')");
  });
});

describe('architecture — no window globals + no bump counter', () => {
  const app = src('components/app.jsx');
  const views = src('components/views.jsx');
  const viewsSec = src('components/views-secondary.jsx');
  const turns = src('components/turns.jsx');
  const panels = src('components/panels.jsx');
  const main = src('main.jsx');

  it('no component reads window.DYSON_DATA or window.DysonLive', () => {
    // Regression for the window-globals + CustomEvent bus that silently
    // dropped renders via the mutate-then-bump() pattern.  The reactive
    // store (src/store/) is the single source of truth now.
    for (const [path, txt] of [
      ['components/app.jsx', app],
      ['components/views.jsx', views],
      ['components/views-secondary.jsx', viewsSec],
      ['components/turns.jsx', turns],
      ['components/panels.jsx', panels],
      ['main.jsx', main],
    ]) {
      expect(txt, `${path} must not read window.DYSON_DATA`).not.toMatch(/window\.DYSON_DATA/);
      expect(txt, `${path} must not read window.DysonLive`).not.toMatch(/window\.DysonLive/);
      expect(txt, `${path} must not read window.__dyson*`).not.toMatch(/window\.__dyson/);
    }
  });

  it('no force-rerender bump counter', () => {
    // The old architecture re-rendered via a useState counter bumped on
    // every mutation.  useSyncExternalStore replaces that — dropping
    // the counter is the whole point of the refactor.
    expect(app, 'app.jsx must not reintroduce bump()').not.toMatch(/const\s+bump\s*=/);
  });

  it('no dyson:* CustomEvent channel in components', () => {
    // The CustomEvent bus (`dyson:live-update`, `dyson:open-artefact`,
    // `dyson:set-conv`, `dyson:open-rail`, `dyson:toggle-artefacts-drawer`)
    // is replaced by store UI nonces.  Sessions' resources map holds
    // the EventSource — no cross-component events needed.
    for (const [path, txt] of [
      ['components/app.jsx', app],
      ['components/views.jsx', views],
      ['components/views-secondary.jsx', viewsSec],
      ['components/turns.jsx', turns],
    ]) {
      expect(txt, `${path} must not dispatch dyson:* CustomEvents`).not.toMatch(/new CustomEvent\(['"]dyson:/);
      expect(txt, `${path} must not listen for dyson:* events`).not.toMatch(/addEventListener\(['"]dyson:/);
    }
  });

  it('main.jsx mounts the React tree inside ApiProvider', () => {
    expect(main).toContain('ApiProvider');
    expect(main).toContain('DysonClient');
    expect(main).toContain('boot(client)');
  });

  it('bridge.js and data.js are gone', () => {
    // These files are the window-globals scaffolding; their removal is
    // the concrete end-state of the refactor.
    const { existsSync } = require('node:fs');
    const { join } = require('node:path');
    const webRoot = join(__dirname, '..');
    expect(existsSync(join(webRoot, 'bridge.js'))).toBe(false);
    expect(existsSync(join(webRoot, 'data.js'))).toBe(false);
  });
});

describe('views-secondary.jsx — artefacts mobile drawer regressions', () => {
  // ArtefactsView / ArtefactReader moved out of views.jsx into
  // views-secondary.jsx when we code-split the non-initial tabs.  The
  // grep-based regressions below follow them.
  const viewsSrc = src('components/views-secondary.jsx');
  const layoutCss = src('styles/layout.css');

  it('ArtefactsView drives .show-side from state, not hardcoded', () => {
    // Regression for "the artefacts sidebar just darkens the screen on
    // mobile" and "tapping an artefact lands on a black screen".  The
    // wrapper used to ship `className="mind show-side"` literally,
    // which on mobile pinned the 80vw drawer over the reader pane and
    // made the reader unreachable.  Now showSide is React state so the
    // drawer can collapse when the user picks an artefact.
    expect(viewsSrc, 'ArtefactsView wrapper must derive class from state')
      .toContain('`mind${showSide ? \' show-side\' : \'\'}`');
    expect(viewsSrc, 'ArtefactsView must not hardcode .mind.show-side')
      .not.toContain('"mind show-side"');
    // Both raw forms (single + double quotes) of the hardcoded class
    // string must be gone.
    expect(viewsSrc).not.toContain("'mind show-side'");
    expect(viewsSrc).toContain('useState(!initialPending)');
  });

  it('ArtefactReader exposes a mobile back button', () => {
    // Reader is now injectable for tests (client prop) but still takes
    // the back-button handler.
    expect(viewsSrc, 'ArtefactReader must accept onShowSide').toMatch(/function ArtefactReader\(\{[^}]*\bonShowSide\b/);
    expect(viewsSrc, 'reader chrome must render the back button').toContain('artefact-back');
    // ArtefactsView must wire the back button up to its setShowSide.
    expect(viewsSrc).toContain('onShowSide={() => setShowSide(true)}');
  });

  it('artefact-back is hidden on desktop, visible on mobile', () => {
    // Desktop: display:none in the base rule so the button doesn't
    // sit next to the title at full width.
    expect(layoutCss).toMatch(/\.artefact-back\s*\{[^}]*display:\s*none/);
    // Mobile: flipped to inline-flex inside the @media (max-width: 760px)
    // block.  Looser regex — just assert the rule exists somewhere
    // after the mobile breakpoint opens.
    const mobileBlock = layoutCss.split('@media (max-width: 760px)')[1] || '';
    expect(mobileBlock, 'mobile media query must show .artefact-back')
      .toMatch(/\.artefact-back\s*\{[^}]*display:\s*inline-flex/);
  });

  it('.mind is a positioning context so the mobile drawer stays below the topbar', () => {
    // Regression for the "Artefacts tab renders as a black screen on
    // mobile" bug.  `.mind-side` is position: absolute on mobile; if
    // its nearest positioned ancestor is the viewport (because .mind
    // is static), the drawer slides over the 44px topbar, covering the
    // nav tabs and the hamburger.  With no visible controls and only
    // low-contrast grey text on a dark drawer, the result read as an
    // unreachable black screen.  Anchor the drawer to .mind itself so
    // it stays inside the body grid cell.
    const rule = layoutCss.match(/\.mind\s*\{([^}]*)\}/);
    expect(rule, '.mind rule must exist').toBeTruthy();
    expect(rule[1], '.mind must be a positioning context for .mind-side')
      .toMatch(/position:\s*relative/);
  });

  it('empty ArtefactReader still exposes the mobile back button', () => {
    // Regression for "the empty reader is a one-way door".  The
    // `if (!id)` branch used to early-return a centered <section>
    // with no title bar at all — so when showSide was false and
    // selected was null (chip pointing at a deleted artefact, race
    // on first paint, etc.), there was no back button to re-open
    // the drawer.  The empty branch must now render the title bar
    // including the back button.
    const emptyBranch = viewsSrc.match(/if\s*\(\s*!id\s*\)\s*\{[\s\S]*?return\s*\([\s\S]*?\);\s*\}/);
    expect(emptyBranch, 'ArtefactReader must have an if (!id) branch').toBeTruthy();
    // The empty branch renders `{back}` — the same JSX handle that the
    // non-empty branch uses, which is the `.artefact-back` button.  The
    // variable indirection is intentional: one definition, reused in
    // both branches of the reader.
    expect(emptyBranch[0], 'empty reader branch must render the back handle')
      .toContain('{back}');
  });

  it('mobile drawer has a tap-to-close scrim', () => {
    // Regression for "I can\'t close the drawer when the list is empty".
    // The only previous way to dismiss the 80vw drawer was to pick an
    // artefact (impossible when the list is empty) — there was no scrim
    // under it.  Now ArtefactsView renders a .mind-scrim when showSide
    // is true, and layout.css makes it a tap target on mobile only.
    expect(viewsSrc, 'ArtefactsView must render .mind-scrim while showSide is true')
      .toMatch(/showSide\s*&&\s*<div\s+className="mind-scrim"/);
    expect(viewsSrc, 'scrim must dismiss the drawer on click')
      .toMatch(/className="mind-scrim"\s+onClick=\{\(\)\s*=>\s*setShowSide\(false\)\}/);
    // Scrim must be hidden on desktop (display:none in the base rule).
    expect(layoutCss).toMatch(/\.mind-scrim\s*\{[^}]*display:\s*none/);
    // ...and shown only on mobile when .mind.show-side is set.
    const mobileBlock = layoutCss.split('@media (max-width: 760px)')[1] || '';
    expect(mobileBlock, 'mobile media query must show .mind.show-side .mind-scrim')
      .toMatch(/\.mind\.show-side\s+\.mind-scrim\s*\{[^}]*display:\s*block/);
  });

  it('mobile drawer is full-width so no dead pane strip shows through', () => {
    // Regression for the "dim broken sidebar" bug visible on iOS.  The
    // drawer used to be `width: 80vw; max-width: 320px`, which left a
    // 20vw strip of empty .mind-pane painting var(--bg-1) next to it —
    // reading as a black void alongside the list.  Clamp removed on
    // mobile; the drawer now covers the full viewport when open.
    const mobileBlock = layoutCss.split('@media (max-width: 760px)')[1] || '';
    const sideRule = mobileBlock.match(/\.mind-side\s*\{([^}]*)\}/);
    expect(sideRule, 'mobile .mind-side rule must exist').toBeTruthy();
    expect(sideRule[1], 'drawer must be full-width on mobile')
      .toMatch(/width:\s*100%/);
    expect(sideRule[1], 'drawer must not re-clamp to 80vw / 320px')
      .not.toMatch(/80vw|320px/);
  });

  it('Artefacts hamburger is not killed by the Mind-view LeftRail-hide rule', () => {
    // Regression for "tapping ☰ on the artefacts tab does nothing".
    // The Mind view body class is `.body no-left no-right` so its
    // redundant LeftRail can be hidden with `.body.no-left.no-right
    // .left { display:none }`.  The previous selector
    // `.body.no-right .left { display:none }` was too loose — it also
    // matched the Artefacts body (`.body no-right` without no-left),
    // overriding the `.body.show-left .left { display:flex }` mobile
    // drawer rule and making the empty-state's "Tap ☰ to switch" hint
    // a lie.
    const mobileBlock = layoutCss.split('@media (max-width: 760px)')[1] || '';
    expect(mobileBlock, 'mobile block must hide LeftRail in Mind view only')
      .toMatch(/\.body\.no-left\.no-right\s+\.left\s*\{[^}]*display:\s*none/);
    expect(mobileBlock, 'old over-broad .body.no-right .left rule must be gone')
      .not.toMatch(/^\s*\.body\.no-right\s+\.left\s*,/m);
    expect(mobileBlock, 'old over-broad .body.no-right .left rule must be gone')
      .not.toMatch(/^\s*\.body\.no-right\s+\.left\s*\{/m);
  });
});

describe('turns.jsx — markdown + composer regressions', () => {
  const turnsSrc = src('components/turns.jsx');

  it('markdown inline-code does not leak control-char placeholders', () => {
    // Regression for the "CODE0 / CODEBLOCK_0 leaked into chat output"
    // bug.  Inline-code placeholders used  / — control chars
    // the DOM strips on innerHTML assignment, so literal placeholder
    // text leaked through.  Fix: tokenise on backticks directly.
    expect(turnsSrc).not.toContain(' CODEBLOCK_');
    expect(turnsSrc).not.toContain('CODE_');
    expect(turnsSrc).toContain('split(/(`[^`]+`)/g)');
  });

  it('markdown header splits from following paragraph', () => {
    // Regression for "## CRITICAL\nNo findings. rendered as literal ## text".
    expect(turnsSrc).toContain("replace(/^(#{1,6}\\s+.+)$/gm, '$1\\n')");
  });

  it('FileBlock renders inline image or download link', () => {
    // Regression for "agent can't deliver files to the UI".  The SSE
    // dispatcher lives in api/stream.js now (a pure function) and the
    // file-event handler is in app.jsx's streamCallbacks().
    const app = src('components/app.jsx');
    const stream = src('api/stream.js');
    expect(turnsSrc).toContain('function FileBlock');
    expect(turnsSrc).toContain("b.type === 'file'");
    expect(stream).toContain("case 'file':");
    expect(app).toContain('onFile:');
  });

  it('Composer uses a real file input, not a fake chip', () => {
    // Regression for "paperclip pretends to attach a file".
    expect(turnsSrc).toContain('type="file"');
    expect(turnsSrc).toContain('function fileToBase64');
    expect(
      !turnsSrc.includes("name: 'screenshot.png'"),
      'hardcoded fake screenshot.png attachment must be gone',
    ).toBe(true);
  });
});
