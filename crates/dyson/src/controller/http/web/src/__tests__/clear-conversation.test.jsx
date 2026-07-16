// Regression: `/clear` used a bare fetch() that omitted the X-Dyson-CSRF
// header, so the controller's CSRF gate rejected it with a 400 and the
// conversation never reset.  It must now route through the API client
// (client.clearConversation → _authedFetch, which stamps CSRF + bearer).

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, screen } from '@testing-library/react';
import { App } from '../components/app.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import {
  setConversations,
  __resetAppStoreForTests,
} from '../store/app.js';
import {
  ensureSession,
  updateSession,
  __resetSessionsForTests,
} from '../store/sessions.js';

function stubClient(overrides = {}) {
  return {
    loadFeedback: vi.fn(async () => []),
    load: vi.fn(async () => ({ turns: [], live: false })),
    createChat: vi.fn(async () => ({ id: 'c-new', title: 'New conversation' })),
    exportConversation: vi.fn(async () => new Blob(['{}'], { type: 'application/json' })),
    postModel: vi.fn(async () => ({})),
    clearConversation: vi.fn(async () => ({ ok: true, cleared: true })),
    send: vi.fn(() => ({ close: vi.fn() })),
    cancel: vi.fn(async () => ({})),
    feedback: vi.fn(async () => ({})),
    ...overrides,
  };
}

beforeEach(() => {
  window.location.hash = '#/c/c1';
  __resetAppStoreForTests();
  __resetSessionsForTests();
  setConversations([{ id: 'c1', title: 'Chat one', live: false, source: 'http' }]);
  ensureSession('c1');
  updateSession('c1', s => ({ ...s, loaded: true }));
});

afterEach(() => {
  cleanup();
  window.location.hash = '';
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

describe('/clear', () => {
  it('routes through client.clearConversation (CSRF-safe), not a bare fetch or a turn', () => {
    const client = stubClient();
    render(
      <ApiProvider client={client}>
        <App/>
      </ApiProvider>
    );

    const textarea = screen.getByPlaceholderText(/Reply to/i);
    fireEvent.change(textarea, { target: { value: '/clear' } });
    // First Enter picks the highlighted slash command (closes the menu);
    // the second Enter actually submits.
    fireEvent.keyDown(textarea, { key: 'Enter' });
    fireEvent.keyDown(textarea, { key: 'Enter' });

    expect(client.clearConversation).toHaveBeenCalledWith('c1');
    // Must NOT go down the normal LLM turn path.
    expect(client.send).not.toHaveBeenCalled();
  });
});
