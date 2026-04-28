/* Regression: first-login UX must surface an empty chat ready to type into.
 *
 * Without this, the SPA renders the chat pane in a "no conversations"
 * empty state and the user has to find and click "+ New Conversation"
 * in a sidebar drawer that's collapsed by default on mobile.
 */
import { describe, it, expect } from 'vitest';

import { boot } from '../api/boot.js';
import { app } from '../store/app.js';

const noopClient = (overrides = {}) => ({
  listConversations: async () => [],
  listProviders:     async () => [],
  getMind:           async () => ({ backend: 'fs', files: [] }),
  getActivity:       async () => ({ lanes: [] }),
  createChat:        async () => ({ id: 'c-0001', title: 'New conversation' }),
  ...overrides,
});

const flush = (ms = 5) => new Promise((r) => setTimeout(r, ms));

describe('boot — first-login auto-create', () => {
  it('mints a conversation when the server returns an empty list', async () => {
    let createCalled = 0;
    const dispose = boot(
      noopClient({
        createChat: async () => { createCalled += 1; return { id: 'c-0001', title: 'x' }; },
      }),
      { pollMs: 1_000_000 },
    );
    await flush();
    expect(createCalled).toBe(1);
    expect(app.getSnapshot().conversations.map((c) => c.id)).toEqual(['c-0001']);
    dispose();
  });

  it('does NOT mint when the server already returned conversations', async () => {
    let createCalled = 0;
    const dispose = boot(
      noopClient({
        listConversations: async () => [{ id: 'c-existing', title: 't', live: false }],
        createChat: async () => { createCalled += 1; return { id: 'should-not-happen' }; },
      }),
      { pollMs: 1_000_000 },
    );
    await flush();
    expect(createCalled).toBe(0);
    expect(app.getSnapshot().conversations.map((c) => c.id)).toEqual(['c-existing']);
    dispose();
  });
});
