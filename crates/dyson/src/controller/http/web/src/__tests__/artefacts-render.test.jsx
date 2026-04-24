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
import { render, fireEvent, cleanup, act } from '@testing-library/react';
import { ArtefactsView, ArtefactReader } from '../components/views-secondary.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import {
  setConversations, requestToggleArtefactsDrawer,
  __resetAppStoreForTests,
} from '../store/app.js';
import { updateSession, __resetSessionsForTests } from '../store/sessions.js';

// Tests mount ArtefactsView inside ApiProvider with a stub client — the
// view calls listArtefacts on collapsed chats it doesn't have data for,
// so the stub returns [] to satisfy the round-trip.
function stubClient() {
  return {
    listArtefacts: async () => [],
    loadArtefact: async () => ({ body: '', chatId: null }),
  };
}

function renderWithApi(ui, client = stubClient()) {
  return render(<ApiProvider client={client}>{ui}</ApiProvider>);
}

function seedChatArtefacts(chatId, artefacts) {
  updateSession(chatId, s => ({ ...s, artefacts, artefactsLoaded: true }));
}

beforeEach(() => {
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

afterEach(() => {
  cleanup();
});

describe('ArtefactsView — tree sidebar', () => {
  it('no chats with artefacts: shows the empty-state copy in the drawer', () => {
    const { container, queryByText } = renderWithApi(
      <ArtefactsView conv={null} setConv={() => {}}/>
    );
    // Drawer is rendered (show-side = true), empty-state copy is in the
    // drawer — not in a standalone .mind-pane as in the previous
    // design.  The copy anchors the screen when nothing else will.
    expect(container.querySelector('.mind.show-side')).toBeTruthy();
    expect(queryByText(/No artefacts yet/)).toBeTruthy();
  });

  it('renders every chat with artefacts, with the active one pre-expanded', () => {
    setConversations([
      { id: 'c1', title: 'Alpha chat',  live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',   live: false, hasArtefacts: true, source: 'http' },
      { id: 'c3', title: 'Gamma chat',  live: false, hasArtefacts: false, source: 'http' },
    ]);
    seedChatArtefacts('c1', [
      { id: 'a1', title: 'First report',  bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedChatArtefacts('c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = renderWithApi(
      <ArtefactsView conv="c1" setConv={() => {}}/>
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
    setConversations([
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',  live: false, hasArtefacts: true, source: 'http' },
    ]);
    seedChatArtefacts('c1', [
      { id: 'a1', title: 'First report', bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedChatArtefacts('c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = renderWithApi(
      <ArtefactsView conv="c1" setConv={() => {}}/>
    );
    const side = container.querySelector('.mind-side');
    expect(side.textContent).not.toContain('Second report');
    const betaRow = [...side.querySelectorAll('.artefact-chat-row')]
      .find(r => r.textContent.includes('Beta chat'));
    fireEvent.click(betaRow);
    // After the click the row expands — c2's artefact shows.
    expect(side.textContent).toContain('Second report');
  });

  it('clicking an artefact in a sibling chat switches conv', () => {
    setConversations([
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',  live: false, hasArtefacts: true, source: 'http' },
    ]);
    seedChatArtefacts('c1', [
      { id: 'a1', title: 'First report',  bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedChatArtefacts('c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    let lastConv = null;
    const setConv = (id) => { lastConv = id; };

    const { container } = renderWithApi(
      <ArtefactsView conv="c1" setConv={setConv}/>
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

    expect(lastConv, 'picking a sibling chat artefact must promote its chat to active').toBe('c2');
    // Drawer must collapse so the reader becomes the visible surface.
    const mind = container.querySelector('.mind');
    expect(mind.classList.contains('show-side')).toBe(false);
  });

  it('topbar hamburger event toggles the drawer', () => {
    setConversations([
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
    ]);
    seedChatArtefacts('c1', [
      { id: 'a1', title: 'First report', bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = renderWithApi(
      <ArtefactsView conv="c1" setConv={() => {}}/>
    );
    // Auto-select collapses the drawer on mount.
    expect(container.querySelector('.mind').classList.contains('show-side')).toBe(false);
    // Fire what TopBar's hamburger bumps on the Artefacts tab.  Wrap
    // in act() so useSyncExternalStore flushes the subscription update
    // and the subsequent useState toggle before the next assertion.
    act(() => { requestToggleArtefactsDrawer(); });
    expect(
      container.querySelector('.mind').classList.contains('show-side'),
      'hamburger must reopen the drawer so the tree is reachable from the reader',
    ).toBe(true);
    act(() => { requestToggleArtefactsDrawer(); });
    expect(container.querySelector('.mind').classList.contains('show-side')).toBe(false);
  });
});

describe('ArtefactReader — empty-id branch stays reachable', () => {
  it('renders the back button even when id is null', () => {
    const { container, queryByText } = renderWithApi(
      <ArtefactReader id={null} onShowSide={() => {}}/>
    );
    expect(container.querySelector('.artefact-back')).toBeTruthy();
    expect(queryByText(/Select an artefact to read/)).toBeTruthy();
  });

  it('does not render the back button when onShowSide is undefined (desktop embed)', () => {
    // onShowSide omitted → back button hidden at the source level, not
    // just via CSS.  Guards against a future refactor that always wires
    // the prop and leaves a dead button next to the title on desktop.
    const { container } = renderWithApi(<ArtefactReader id={null}/>);
    expect(container.querySelector('.artefact-back')).toBeNull();
  });
});
