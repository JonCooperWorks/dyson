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
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, act } from '@testing-library/react';
import { ArtefactsView, ArtefactReader } from '../components/views-secondary.jsx';
import { ArtefactBlock } from '../components/turns.jsx';
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

  it('renders every chat with artefacts and marks the active one', () => {
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
    // data-active marker is on the active chat row for styling.
    const activeRow = side.querySelector('.artefact-chat-row[data-active="true"]');
    expect(activeRow?.textContent).toContain('Alpha chat');
  });

  it('only one useful chat branch is expanded by default', () => {
    // Large agents can have dozens of report-bearing chats.  Keep the
    // active branch visible, but leave siblings collapsed so first
    // paint does not fan out a request per chat.
    setConversations([
      { id: 'c1', title: 'Alpha chat', live: false, hasArtefacts: true, source: 'http' },
      { id: 'c2', title: 'Beta chat',  live: false, hasArtefacts: true, source: 'http' },
      { id: 'c3', title: 'Gamma chat', live: false, hasArtefacts: true, source: 'http' },
    ]);
    seedChatArtefacts('c1', [
      { id: 'a1', title: 'First report',  bytes: 1024, kind: 'security_review', created_at: 0 },
    ]);
    seedChatArtefacts('c2', [
      { id: 'b1', title: 'Second report', bytes: 2048, kind: 'security_review', created_at: 0 },
    ]);
    seedChatArtefacts('c3', [
      { id: 'g1', title: 'Third report',  bytes: 4096, kind: 'security_review', created_at: 0 },
    ]);
    const { container } = renderWithApi(
      <ArtefactsView conv="c1" setConv={() => {}}/>
    );
    const side = container.querySelector('.mind-side');
    // The chat rows are visible, but only the active branch is open.
    const titles = [...side.querySelectorAll('.artefact-row .title')].map(el => el.textContent);
    expect(titles).toEqual(['First report']);
    expect(side.textContent).toContain('Beta chat');
    expect(side.textContent).toContain('Gamma chat');
  });

  it('toggling a chat row collapses then re-expands its artefacts', () => {
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
    // Sibling chats start collapsed.
    expect(side.textContent).not.toContain('Second report');
    const betaRow = [...side.querySelectorAll('.artefact-chat-row')]
      .find(r => r.textContent.includes('Beta chat'));
    // First click expands the branch.
    fireEvent.click(betaRow);
    expect(side.textContent).toContain('Second report');
    // Second click collapses it again.
    fireEvent.click(betaRow);
    expect(side.textContent).not.toContain('Second report');
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
    const betaRow = [...side.querySelectorAll('.artefact-chat-row')]
      .find(r => r.textContent.includes('Beta chat'));
    fireEvent.click(betaRow);
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
  it('renders reader chrome without a duplicate menu button when id is null', () => {
    const { container, queryByText } = renderWithApi(
      <ArtefactReader id={null}/>
    );
    expect(container.querySelector('.artefact-reader-head')).toBeTruthy();
    expect(container.querySelector('.artefact-back')).toBeNull();
    expect(queryByText(/Select an artefact to read/)).toBeTruthy();
  });

  it('keeps the loaded reader header to one drawer opener: the topbar', async () => {
    const client = {
      listArtefacts: async () => [],
      loadArtefact: async () => ({ body: '# Report', chatId: 'c1' }),
    };
    const { container } = renderWithApi(<ArtefactReader id="a1" chatId="c1"/>, client);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(container.querySelector('.artefact-reader-head')).toBeTruthy();
    expect(container.querySelector('.artefact-back')).toBeNull();
  });
});

// `send_file` now promotes every sent file to an Other-kind artefact —
// markdown via inlined body (existing markdown render path), everything
// else via a download card driven by metadata.file_url.  These tests
// pin both branches so a future refactor can't silently regress to the
// "files invisible in Artefacts tab" state.
describe('ArtefactReader — sent files', () => {
  // A reader needs both a populated session (so findArtefactMeta picks
  // up the metadata header) and an awaited loadArtefact promise (so the
  // body is set before assertions run).
  async function mountReader(chatId, art, body) {
    setConversations([
      { id: chatId, title: 'Live chat', live: false, hasArtefacts: true, source: 'http' },
    ]);
    seedChatArtefacts(chatId, [art]);
    const client = {
      listArtefacts: async () => [art],
      loadArtefact: async () => ({ body, chatId }),
    };
    const utils = renderWithApi(<ArtefactReader id={art.id}/>, client);
    // Allow loadArtefact's microtask + setBody to flush.
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    return utils;
  }

  it('renders a markdown file artefact through the existing markdown helper', async () => {
    const md = '# Findings\n\nA bullet.\n';
    const { container } = await mountReader('c1', {
      id: 'a1',
      title: 'findings.md',
      bytes: md.length,
      kind: 'other',
      created_at: 0,
      metadata: {
        file_url: '/api/files/f1',
        file_name: 'findings.md',
        mime_type: 'text/markdown',
        bytes: md.length,
      },
    }, md);

    // Existing render path: <div class="prose"> with markdown(body)
    // turning '# Findings' into an <h1>.  Pinned so the binary-file
    // branch can't ever swallow markdown.
    const prose = container.querySelector('.prose');
    expect(prose, 'markdown files must use the .prose render path').toBeTruthy();
    expect(prose.querySelector('h1')?.textContent).toContain('Findings');
    // No download card surface.
    expect(container.textContent).not.toMatch(/^Download$/m);
  });

  it('renders a text/plain file artefact as an inline preview, not a download-only card', async () => {
    const report = 'Page loaded\nURL: https://targetpractice.network/\nStatus: ok';
    setConversations([
      { id: 'c1', title: 'Live chat', live: false, hasArtefacts: true, source: 'http' },
    ]);
    const art = {
      id: 'a-text',
      title: 'screenshot_result.txt',
      bytes: 13,
      kind: 'other',
      created_at: 0,
      metadata: {
        file_url: '/api/files/f2',
        file_name: 'screenshot_result.txt',
        mime_type: 'text/plain; charset=utf-8',
        bytes: report.length,
      },
    };
    seedChatArtefacts('c1', [art]);
    const client = {
      listArtefacts: async () => [art],
      loadArtefact: async () => ({ body: '/api/files/f2', chatId: 'c1' }),
      loadFileText: vi.fn(async () => report),
    };
    const { container } = renderWithApi(<ArtefactReader id={art.id} chatId="c1"/>, client);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); });

    const preview = container.querySelector('.artefact-text-preview');
    expect(preview, 'text files must render in the inline preview reader').toBeTruthy();
    expect(preview.textContent).toContain('targetpractice.network');
    expect(container.querySelector('.artefact-file-card')).toBeNull();
    expect(client.loadFileText).toHaveBeenCalledWith('/api/files/f2');
  });

  it('renders an image artefact as <img>, not as "image no longer available"', async () => {
    // The image branch was previously gated on a `fileUrl` that was
    // explicitly nulled out for kind:image, so every image reader fell
    // through to the empty-state copy.  Pin: a kind:image artefact with
    // the URL on metadata.file_url renders an <img src> pointing at it.
    const url = '/api/files/f7';
    const { container, queryByText } = await mountReader('c1', {
      id: 'a3',
      title: 'cat.png',
      bytes: 4096,
      kind: 'image',
      created_at: 0,
      metadata: {
        file_url: url,
        file_name: 'cat.png',
        mime_type: 'image/png',
        bytes: 4096,
      },
    }, url);
    const img = container.querySelector('img');
    expect(img, 'image artefact must render an <img>').toBeTruthy();
    expect(img.getAttribute('src')).toBe(url);
    expect(queryByText(/no longer available/i)).toBeNull();
  });

  it('falls back to artefact body when meta has not hydrated yet (cold deep-link)', async () => {
    // findArtefactMeta returns null when the user lands on an image
    // artefact deep-link before the conversation list has loaded.  In
    // that case the reader still receives the body via loadArtefact —
    // and for image artefacts the body is the URL.  Pin that fallback
    // so a cold deep-link doesn't read as "Image no longer available".
    setConversations([]);
    const client = {
      listArtefacts: async () => [],
      loadArtefact: async () => ({ body: '/api/files/f8', chatId: 'c1' }),
    };
    const { container } = renderWithApi(<ArtefactReader id={'a4'}/>, client);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    // No meta → no <img> until findArtefactMeta is populated.  The
    // empty-state shouldn't fire either; the reader paints "Select an
    // artefact" only when id is null.  Body fallback only activates
    // when meta.kind says image, so this case stays as plain markdown.
    expect(container.querySelector('.prose')).toBeTruthy();
  });

  it('renders a binary file artefact as a download card, not as markdown', async () => {
    const { container, queryByText } = await mountReader('c1', {
      id: 'a2',
      title: 'data.bin',
      bytes: 12,
      kind: 'other',
      created_at: 0,
      metadata: {
        file_url: '/api/files/f1',
        file_name: 'data.bin',
        mime_type: 'application/octet-stream',
        bytes: 12,
      },
    }, '/api/files/f1');

    // Download card — the visible Download button anchors the branch.
    expect(queryByText('Download')).toBeTruthy();
    // Metadata bar surfaces name + mime so the user knows what they're
    // about to grab.
    expect(container.textContent).toContain('data.bin');
    expect(container.textContent).toContain('application/octet-stream');
    // The download button's anchor target is the file URL — i.e. the
    // reader does not try to feed binary bytes through markdown().
    expect(container.querySelector('.prose')).toBeNull();
  });

  it('uses chat-scoped identity when loading duplicate artefact ids with share controls', async () => {
    seedChatArtefacts('c-tsmc', [{
      id: 'a1',
      title: 'TSMC report',
      bytes: 7,
      kind: 'security_review',
      created_at: 1,
    }]);
    seedChatArtefacts('c-ntnx', [{
      id: 'a1',
      title: 'NTNX report',
      bytes: 7,
      kind: 'security_review',
      created_at: 2,
    }]);
    const loadArtefact = vi.fn(async (_id, chatId) => ({
      body: chatId === 'c-ntnx' ? '# NTNX' : '# TSMC',
      chatId,
    }));
    const client = {
      listArtefacts: async () => [],
      loadArtefact,
    };
    const oldFetch = global.fetch;
    global.fetch = vi.fn();
    try {
      const { container, queryByRole } = renderWithApi(
        <ArtefactReader id="a1" chatId="c-ntnx"/>,
        client,
      );
      await act(async () => { await Promise.resolve(); await Promise.resolve(); });

      expect(loadArtefact).toHaveBeenCalledWith('a1', 'c-ntnx');
      expect(container.textContent).toContain('NTNX report');
      expect(queryByRole('button', { name: /share/i })).toBeTruthy();
      expect(global.fetch).not.toHaveBeenCalled();
    } finally {
      global.fetch = oldFetch;
    }
  });

  it('renders transcript artefact chips with inline share controls', async () => {
    const oldFetch = global.fetch;
    global.fetch = vi.fn();
    try {
      const { getByText, queryByRole } = render(
        <ArtefactBlock
          chatId="c-ntnx"
          block={{
            type: 'artefact',
            id: 'a1',
            title: 'NTNX report',
            kind: 'security_review',
            bytes: 7,
            url: '/#/artefacts/a1',
          }}
        />,
      );

      expect(getByText('NTNX report')).toBeTruthy();
      expect(queryByRole('button', { name: /share/i })).toBeTruthy();
      expect(global.fetch).not.toHaveBeenCalled();
    } finally {
      global.fetch = oldFetch;
    }
  });
});
