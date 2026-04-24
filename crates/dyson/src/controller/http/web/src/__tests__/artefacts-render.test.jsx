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

// Minimal DYSON_DATA + DysonLive stubs.  Tests set `session.artefacts`
// directly so we don't need to round-trip through fetch.
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
  window.DysonLive = {
    listArtefacts: async () => [],
    loadArtefact: async () => ({ body: '', chatId: null }),
  };
  // markdown() is called by ArtefactReader when rendering a body.
  // Stub returns the string through untouched so we can assert on it.
  window.markdown = (s) => s;
  delete window.__dysonOpenArtefactId;
});

afterEach(() => {
  cleanup();
  delete window.__dysonSessions;
});

describe('ArtefactsView — empty states render visible copy', () => {
  it('no conv selected: renders the "Select a conversation" landing copy', () => {
    const { container, queryByText } = render(
      <ArtefactsView conv={null} session={null} bump={() => {}}/>
    );
    // The message must be in the DOM — grep passed last time but the
    // branch was unreachable, leaving a blank <section>.
    expect(queryByText(/Select a conversation to see its artefacts/)).toBeTruthy();
    // And there must be a .mind-pane container holding it so the flex-
    // center rule applies.  A loose text match would pass even if the
    // wrapper was missing and the copy rendered adrift in document.body.
    expect(container.querySelector('section.mind-pane')).toBeTruthy();
  });

  it('conv with 0 artefacts: renders drawer in show-side mode with the empty-state copy', () => {
    const session = { artefacts: [], artefactsLoaded: true, panels: [], es: null };
    const { container, queryByText } = render(
      <ArtefactsView conv="chat-1" session={session} bump={() => {}}/>
    );
    // `.mind.show-side` is the signal that the mobile drawer is covering
    // the pane.  If showSide drifted to false on mount the reader would
    // be visible behind an invisible sidebar — the "tap Artefacts, see
    // nothing useful" user report.
    const mind = container.querySelector('.mind.show-side');
    expect(mind, 'wrapper must carry show-side on an empty list').toBeTruthy();
    expect(queryByText(/No artefacts yet in this chat/)).toBeTruthy();
    // The tap-to-close scrim must be in the DOM while showSide is true.
    expect(container.querySelector('.mind-scrim')).toBeTruthy();
  });

  it('conv with artefacts: list renders, clicking a row collapses the drawer', () => {
    const session = {
      artefacts: [
        { id: 'a0', title: 'Latest report', bytes: 1024, kind: 'security_review', created_at: 0 },
        { id: 'a1', title: 'Older report',  bytes: 2048, kind: 'security_review', created_at: 0 },
      ],
      artefactsLoaded: true,
      panels: [],
      es: null,
    };
    const { container, getByText } = render(
      <ArtefactsView conv="chat-1" session={session} bump={() => {}}/>
    );
    // Both list rows must be rendered, not just the auto-selected one.
    expect(getByText('Latest report')).toBeTruthy();
    expect(getByText('Older report')).toBeTruthy();
    // Auto-select runs in a mount effect — by the time render() returns,
    // setSelected(first) + setShowSide(false) should have applied.
    const mindAfterAuto = container.querySelector('.mind');
    expect(mindAfterAuto, '.mind must be rendered').toBeTruthy();
    expect(
      mindAfterAuto.classList.contains('show-side'),
      'auto-select must collapse the mobile drawer so the reader is visible',
    ).toBe(false);
    // Clicking the second row must fire the open-artefact event.
    let lastEvent = null;
    const h = (e) => { lastEvent = e.detail && e.detail.id; };
    window.addEventListener('dyson:open-artefact', h);
    fireEvent.click(getByText('Older report'));
    window.removeEventListener('dyson:open-artefact', h);
    expect(lastEvent).toBe('a1');
  });
});

describe('ArtefactReader — empty-id branch stays reachable', () => {
  it('renders the back button even when id is null', () => {
    const onShowSide = () => {};
    const { container, queryByText } = render(
      <ArtefactReader id={null} onShowSide={onShowSide}/>
    );
    // Back button is the only way off the reader on mobile.  The
    // pre-fix `if (!id)` branch returned just a centered <section> with
    // no title bar — users were stuck on an empty reader with no way
    // to reopen the drawer.
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
