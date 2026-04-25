// UX tweaks:
//   1. Copy-from-anywhere: the per-turn copy button used to scroll out
//      of view inside long agent messages — `.turn .who` is now `position:
//      sticky` so the header (with its copy button) stays pinned to the
//      transcript top while you scroll a turn.
//   2. Tool-pane scroll: clicking a tool chip pushes `#/c/<id>/t/<ref>`
//      and opens the panel, but if the panel was below the rail's fold
//      the user saw nothing happen.  RightRail now scrolls the matching
//      panel into view whenever `session.openTool` flips.
//
// jsdom has no CSS engine and no layout, so the sticky header is
// asserted via a source-text grep on turns.css (matches the regression
// test style next door).  The scroll wiring is exercised with a real
// React mount — we seed the session store, render RightRail, and assert
// `Element.prototype.scrollIntoView` is invoked on the panel that
// matches `openTool`.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { render, cleanup, act, waitFor } from '@testing-library/react';
import { RightRail } from '../components/views.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { setTool, __resetAppStoreForTests } from '../store/app.js';
import {
  ensureSession, updateSession, openPanel, closePanel,
  __resetSessionsForTests,
} from '../store/sessions.js';
// Force the lazy ToolPanel module to load eagerly under vitest.  The
// production lazy() boundary still works at runtime; this just removes
// the Suspense delay from the test's render → assert window so we
// don't have to thread waitFor() through every assertion.
import '../components/panels.jsx';

const __dirname = dirname(fileURLToPath(import.meta.url));
const turnsCss = () => readFileSync(join(__dirname, '..', 'styles', 'turns.css'), 'utf8');

function stubClient() {
  // RightRail polls /api/activity for running subagents; the stub keeps
  // the network silent so the test only watches the panel-scroll path.
  return { getActivity: async () => ({ lanes: [] }) };
}

function renderRail(chatId, client = stubClient()) {
  return render(
    <ApiProvider client={client}><RightRail chatId={chatId}/></ApiProvider>
  );
}

beforeEach(() => {
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

afterEach(() => {
  cleanup();
});

describe('UX: copy-from-anywhere — sticky copy button', () => {
  it('.turn .copy-turn is position: sticky so it stays reachable mid-scroll', () => {
    const css = turnsCss();
    // First definition of .turn .copy-turn (later overrides for hover/
    // touch shouldn't redeclare position).  Match from the selector to
    // the next `}` so the assertion targets that block specifically.
    const block = css.match(/\.turn \.copy-turn \{[^}]*\}/);
    expect(block, '.turn .copy-turn selector must exist in turns.css').toBeTruthy();
    expect(block[0]).toMatch(/position:\s*sticky/);
    expect(block[0]).toMatch(/top:/);
    // Background keeps the sticky button readable over scrolling prose.
    expect(block[0]).toMatch(/background:/);
  });

  it('.turn .who is no longer sticky — only the copy button pins', () => {
    // Regression: the original attempt made `.who` sticky, which
    // dragged the whole header bar across the prose.  Lock that out.
    const css = turnsCss();
    const block = css.match(/\.turn \.who \{[^}]*\}/);
    expect(block, '.turn .who selector must exist in turns.css').toBeTruthy();
    expect(block[0]).not.toMatch(/position:\s*sticky/);
  });
});

describe('UX: tool pane scroll — RightRail follows session.openTool', () => {
  let originalScroll;

  beforeEach(() => {
    // jsdom doesn't implement scrollIntoView; spy on it so the layout
    // effect doesn't crash and we can assert the call.
    originalScroll = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = vi.fn();
  });

  afterEach(() => {
    Element.prototype.scrollIntoView = originalScroll;
  });

  function seed(chatId, refs, openRef) {
    // Seed app.tools and the session.panels stack as if the user had
    // clicked these tool chips in order, with `openRef` being the most
    // recent click. ensureSession + updateSession mirror what
    // streamCallbacks does in app.jsx; openPanel is the same reducer
    // handleOpenTool calls so this reproduces the exact runtime path.
    ensureSession(chatId);
    for (const ref of refs) {
      setTool(ref, { name: ref, icon: 'X', sig: '', dur: '', exit: 'ok',
                     status: 'done', kind: 'fallback', body: { text: ref } });
      updateSession(chatId, s => openPanel(s, ref));
    }
    if (openRef && refs[refs.length - 1] !== openRef) {
      updateSession(chatId, s => openPanel(s, openRef));
    }
  }

  async function findPanel(container, ref) {
    return waitFor(() => {
      const node = container.querySelector(`[data-tool-ref="${ref}"]`);
      if (!node) throw new Error(`panel ${ref} not yet mounted`);
      return node;
    });
  }

  it('renders each panel with a data-tool-ref attribute matching its ref', async () => {
    seed('c-1', ['c-1-live-1', 'c-1-live-2'], 'c-1-live-2');
    const { container } = renderRail('c-1');
    await findPanel(container, 'c-1-live-2');
    const panels = container.querySelectorAll('.r-stack [data-tool-ref]');
    expect([...panels].map(p => p.getAttribute('data-tool-ref')))
      .toEqual(['c-1-live-1', 'c-1-live-2']);
  });

  it('scrolls the openTool panel into view after mount', async () => {
    seed('c-1', ['c-1-live-1', 'c-1-live-2', 'c-1-live-3'], 'c-1-live-3');
    const { container } = renderRail('c-1');
    const target = await findPanel(container, 'c-1-live-3');
    await waitFor(() => expect(target.scrollIntoView).toHaveBeenCalled());
  });

  it('scrolls the newly-opened panel into view when openTool changes', async () => {
    seed('c-1', ['c-1-live-1', 'c-1-live-2'], 'c-1-live-2');
    const { container } = renderRail('c-1');
    await findPanel(container, 'c-1-live-2');
    Element.prototype.scrollIntoView.mockClear();
    // Simulate the user clicking the chip for the first panel — the
    // session reducer flips openTool back to live-1.
    await act(async () => {
      updateSession('c-1', s => openPanel(s, 'c-1-live-1'));
    });
    const target = await findPanel(container, 'c-1-live-1');
    await waitFor(() => expect(target.scrollIntoView).toHaveBeenCalled());
  });

  it('does not scroll when openTool is null (closed panel, no active tool)', async () => {
    seed('c-1', ['c-1-live-1'], 'c-1-live-1');
    const { container } = renderRail('c-1');
    await findPanel(container, 'c-1-live-1');
    Element.prototype.scrollIntoView.mockClear();
    await act(async () => {
      // closePanel clears openTool when the closed ref was the active one.
      updateSession('c-1', s => closePanel(s, 'c-1-live-1'));
    });
    await waitFor(() => {
      expect(container.querySelector('[data-tool-ref]')).toBeNull();
    });
    expect(Element.prototype.scrollIntoView).not.toHaveBeenCalled();
  });
});
