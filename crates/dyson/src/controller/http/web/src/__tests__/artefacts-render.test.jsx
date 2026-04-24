// Real render tests for ArtefactsView / ArtefactReader.
//
// The source-text greps in regression.test.js missed the "black screen on
// mobile" bug five times in a row because they verified what the source
// said, not what React actually rendered.  These tests mount the
// components under jsdom + @testing-library/react and walk the resulting
// DOM — they catch missing elements, wrong branches, and event-handler
// regressions without pretending to measure layout (jsdom has no layout
// engine; a Playwright test would be needed for pixel-level checks).

import React from 'react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, cleanup } from '@testing-library/react';
import { ArtefactsView, ArtefactReader } from '../components/views.jsx';

// App owns the sessions map in the real app; we mirror that here so
// ArtefactsView's tree can read cached artefacts and findArtefactMeta
// can look up titles without a round-trip.
function seedSession(sessions, chatId, artefacts) {
  sessions.set(chatId, { artefacts, artefactsLoaded: true });
}

beforeEach(() => {
  window.DYSON_DATA = {
    activeModel: '',
    conversations: { http: [], telegram: [], swarm: [] },
    convo: [],
    tools: {},
    subagents: {},
    providers: [],
    activity: [],
    checkpoints: [],
    mind: { backend: '', heartbeat: '', files: [], open: { path: '', content: '', recentEdits: [] } },
    skills: { builtin: [], denials: [], mcp: [] },
    slashCmds: [],
  };
  window.__dysonSessions = new Map();
  window.DysonLive = {
    // listArtefacts gets called by ensureArtefacts for chats without a
    // pre-seeded session.  Tests that care about a specific chat seed
    // its artefacts via window.__dysonSessions instead.
    listArtefacts: async () => [],
    loadArtefact: async () => ({ body: '', chatId: null }),
  };
  delete window.__dysonOpenArtefactId;
});

afterEach(() => {
  cleanup();
  delete window.__dysonSessions;
});

describe('ArtefactsView — tree sidebar', () => {
  it('no chats with artefacts: shows the empty-state copy in the drawer', () => {
    const { container, queryByText } = render(
      <ArtefactsView conv={null} setConv={() => {}} bump={() => {}}/>
    );
    // Drawer is rendered (show-side = true), empty-state copy is in the
    // drawer — not in a standalone .mind-pane as in the previous
    // design.  The copy anchors the screen when nothing else will.
    expect(container.querySelector('.mind.show-side')).toBeTruthy();
    expect(queryByText(/No artefacts yet/)).toBeTruthy();
  });

  it('renders every chat with artefacts, with the active one pre-expanded', () => {
    window.DYSON_DATA.conversations.http = [
      { id: 'c1', title: 'Alpha chat',  live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',   live: false, hasArtefacts: true, source: 'http' },
      { id: 'c3', title: 'Gamma chat',  live: false, hasArtefacts: false, source: 'http' },
    ];
    seedSession(window.__dysonSessions, 'c1', [
      { id: 'a1', title: 'First report',  bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedSession(window.__dysonSessions, 'c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = render(
      <ArtefactsView conv="c1" setConv={() => {}} bump={() => {}}/>
    );
    // Scope assertions to the sidebar — the reader also renders the
    // active artefact's title, which would trip a global text query
    // once auto-select kicks in.
    const side = container.querySelector('.mind-side');
    const sideText = side.textContent;
    // c3 has no artefacts; it must be filtered out of the tree.
    expect(sideText).not.toContain('Gamma chat');
    expect(sideText).toContain('Alpha chat');
    expect(sideText).toContain('Beta chat');
    // The ACTIVE chat (c1) is pre-expanded: its artefact row is
    // visible.  c2 stays collapsed.
    expect(side.querySelector('.artefact-row .title')?.textContent).toBe('First report');
    expect(sideText).not.toContain('Second report');
    // data-active marker is on the active chat row for styling.
    const activeRow = side.querySelector('.artefact-chat-row[data-active="true"]');
    expect(activeRow?.textContent).toContain('Alpha chat');
  });

  it('clicking a collapsed chat row expands it and reveals its artefacts', () => {
    window.DYSON_DATA.conversations.http = [
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',  live: false, hasArtefacts: true, source: 'http' },
    ];
    seedSession(window.__dysonSessions, 'c1', [
      { id: 'a1', title: 'First report', bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedSession(window.__dysonSessions, 'c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = render(
      <ArtefactsView conv="c1" setConv={() => {}} bump={() => {}}/>
    );
    const side = container.querySelector('.mind-side');
    expect(side.textContent).not.toContain('Second report');
    // Find the Beta chat row specifically (by title text in row).
    const betaRow = [...side.querySelectorAll('.artefact-chat-row')]
      .find(r => r.textContent.includes('Beta chat'));
    fireEvent.click(betaRow);
    // After the click the row expands — c2's artefact shows.
    expect(side.textContent).toContain('Second report');
  });

  it('clicking an artefact in a sibling chat switches conv + fires the navigation event', () => {
    window.DYSON_DATA.conversations.http = [
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',  live: false, hasArtefacts: true, source: 'http' },
    ];
    seedSession(window.__dysonSessions, 'c1', [
      { id: 'a1', title: 'First report',  bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedSession(window.__dysonSessions, 'c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    let lastConv = null;
    const setConv = (id) => { lastConv = id; };
    const events = [];
    const h = (e) => { events.push(e.detail && e.detail.id); };
    window.addEventListener('dyson:open-artefact', h);

    const { container } = render(
      <ArtefactsView conv="c1" setConv={setConv} bump={() => {}}/>
    );
    const side = container.querySelector('.mind-side');
    // Expand c2 first — collapsed rows don't expose their artefacts.
    const betaRow = [...side.querySelectorAll('.artefact-chat-row')]
      .find(r => r.textContent.includes('Beta chat'));
    fireEvent.click(betaRow);
    // Now click the artefact inside c2's expanded body.
    const secondArtRow = [...side.querySelectorAll('.artefact-row')]
      .find(r => r.textContent.includes('Second report'));
    fireEvent.click(secondArtRow);
    window.removeEventListener('dyson:open-artefact', h);

    expect(lastConv, 'picking a sibling chat artefact must promote its chat to active').toBe('c2');
    expect(
      events.includes('b1'),
      'the navigation event must carry the picked artefact id',
    ).toBe(true);
    // Drawer must collapse so the reader becomes the visible surface.
    const mind = container.querySelector('.mind');
    expect(mind.classList.contains('show-side')).toBe(false);
  });

  it('topbar hamburger event toggles the drawer', () => {
    window.DYSON_DATA.conversations.http = [
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
    ];
    seedSession(window.__dysonSessions, 'c1', [
      { id: 'a1', title: 'First report', bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = render(
      <ArtefactsView conv="c1" setConv={() => {}} bump={() => {}}/>
    );
    // Auto-select collapses the drawer on mount.
    expect(container.querySelector('.mind').classList.contains('show-side')).toBe(false);
    // Dispatch what TopBar's hamburger fires on the Artefacts tab.
    fireEvent(window, new CustomEvent('dyson:toggle-artefacts-drawer'));
    expect(
      container.querySelector('.mind').classList.contains('show-side'),
      'hamburger must reopen the drawer so the tree is reachable from the reader',
    ).toBe(true);
    fireEvent(window, new CustomEvent('dyson:toggle-artefacts-drawer'));
    expect(container.querySelector('.mind').classList.contains('show-side')).toBe(false);
  });
});

describe('ArtefactReader — empty-id branch stays reachable', () => {
  it('renders the back button even when id is null', () => {
    const onShowSide = () => {};
    const { container, queryByText } = render(
      <ArtefactReader id={null} onShowSide={onShowSide}/>
    );
    expect(container.querySelector('.artefact-back')).toBeTruthy();
    expect(queryByText(/Select an artefact to read/)).toBeTruthy();
  });

  it('does not render the back button when onShowSide is undefined (desktop embed)', () => {
    // onShowSide omitted → back button hidden at the source level, not
    // just via CSS.  Guards against a future refactor that always wires
    // the prop and leaves a dead button next to the title on desktop.
    const { container } = render(<ArtefactReader id={null}/>);
    expect(container.querySelector('.artefact-back')).toBeNull();
  });
});
