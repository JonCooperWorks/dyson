// Mobile new-chat affordance.
//
// The "+ New conversation" button lives in the LeftRail, which is
// display:none on mobile until the sidebar is opened — so on a phone you
// had to open the drawer just to start a chat.  The TopBar now carries a
// new-chat button (paired with the hamburger) that is reachable without
// opening the sidebar.  These tests pin that the affordance exists in the
// topbar, that clicking it creates a conversation without forcing the
// rail open, and that the LeftRail/⌘K/topbar all share one handler.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, waitFor } from '@testing-library/react';
import { App } from '../components/app.jsx';
import { LeftRail } from '../components/views.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { setConversations, __resetAppStoreForTests } from '../store/app.js';
import { __resetSessionsForTests } from '../store/sessions.js';

function stubClient(over = {}) {
  return {
    loadFeedback: vi.fn(async () => []),
    load: vi.fn(async () => ({ messages: [], live: false })),
    createChat: vi.fn(async () => ({ id: 'c-new', title: 'New conversation' })),
    postModel: vi.fn(async () => ({})),
    send: vi.fn(() => ({ close: vi.fn() })),
    cancel: vi.fn(async () => ({})),
    feedback: vi.fn(async () => ({})),
    exportConversation: vi.fn(async () => new Blob(['{}'])),
    ...over,
  };
}

function renderApp(client = stubClient()) {
  return render(<ApiProvider client={client}><App/></ApiProvider>);
}

beforeEach(() => {
  window.location.hash = '#/';
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

afterEach(() => {
  cleanup();
  window.location.hash = '';
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

describe('TopBar — mobile new-chat affordance', () => {
  it('exposes a new-chat button in the topbar, outside the sidebar', () => {
    const { container } = renderApp();
    const btn = container.querySelector('.topbar .new-chat');
    expect(btn, 'topbar must carry a new-chat button reachable without the rail').toBeTruthy();
    // Not nested inside the (mobile-hidden) left rail.
    expect(btn.closest('.left'), 'new-chat button must not live in the sidebar').toBeNull();
    expect(btn.getAttribute('aria-label')).toMatch(/new conversation/i);
  });

  it('clicking it creates a conversation without opening the left rail', async () => {
    const client = stubClient();
    const { container } = renderApp(client);
    // Sidebar starts closed (mobile default).
    expect(container.querySelector('.body.show-left')).toBeFalsy();

    fireEvent.click(container.querySelector('.topbar .new-chat'));

    await waitFor(() => expect(client.createChat).toHaveBeenCalledWith('New conversation'));
    // The rail was never forced open to get here.
    expect(container.querySelector('.body.show-left')).toBeFalsy();
  });
});

describe('LeftRail — shared new-conversation handler', () => {
  it('uses the injected onNew handler when provided', () => {
    setConversations([]);
    const onNew = vi.fn();
    const { container } = render(
      <ApiProvider client={stubClient()}>
        <LeftRail active={null} setActive={() => {}} onNew={onNew}/>
      </ApiProvider>
    );
    fireEvent.click(container.querySelector('.newc .btn'));
    expect(onNew).toHaveBeenCalledTimes(1);
  });

  it('falls back to a local create when rendered standalone (no onNew)', async () => {
    setConversations([]);
    const client = stubClient();
    const { container } = render(
      <ApiProvider client={client}>
        <LeftRail active={null} setActive={() => {}}/>
      </ApiProvider>
    );
    fireEvent.click(container.querySelector('.newc .btn'));
    await waitFor(() => expect(client.createChat).toHaveBeenCalledWith('New conversation'));
  });
});
