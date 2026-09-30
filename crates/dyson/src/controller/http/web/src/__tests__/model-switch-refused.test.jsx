// A model switch Swarm refuses (a model outside the agent's allowed models)
// used to vanish into the console. The menu now stays open and says why.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, screen, waitFor } from '@testing-library/react';
import { App } from '../components/app.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { setProviders, __resetAppStoreForTests } from '../store/app.js';
import { __resetSessionsForTests } from '../store/sessions.js';

const REFUSAL = 'This agent may not use openai/gpt-4o. Its allowed models are set in Swarm; the active model was not changed.';

function stubClient(over = {}) {
  return {
    loadFeedback: vi.fn(async () => []),
    load: vi.fn(async () => ({ messages: [], live: false })),
    createChat: vi.fn(async () => ({ id: 'c-new', title: 'New conversation' })),
    postModel: vi.fn(async () => { throw new Error(REFUSAL); }),
    listModels: vi.fn(async () => ({ provider: 'openrouter', models: [] })),
    send: vi.fn(() => ({ close: vi.fn() })),
    cancel: vi.fn(async () => ({})),
    feedback: vi.fn(async () => ({})),
    exportConversation: vi.fn(async () => new Blob(['{}'])),
    ...over,
  };
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

describe('model switch refused by Swarm', () => {
  it('keeps the menu open and shows the reason', async () => {
    setProviders([
      { id: 'openrouter', name: 'Swarm', active: true, models: ['anthropic/claude-sonnet-4', 'openai/gpt-4o'] },
    ], 'anthropic/claude-sonnet-4');
    const client = stubClient();
    const { container } = render(<ApiProvider client={client}><App/></ApiProvider>);

    fireEvent.click(container.querySelector('.provider-select'));
    fireEvent.click(await screen.findByText('openai/gpt-4o'));

    await waitFor(() => expect(client.postModel).toHaveBeenCalledWith('openrouter', 'openai/gpt-4o'));
    expect((await screen.findByRole('alert')).textContent).toBe(REFUSAL);
    expect(container.querySelector('.modelmenu')).toBeTruthy();
  });
});
