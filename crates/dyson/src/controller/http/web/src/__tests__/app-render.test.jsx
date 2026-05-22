// Regression coverage for the production blank page caused by
// ConversationView referencing slashCommands outside its scope.  Mount
// the real App, seed backend-provided commands, and drive the composer
// far enough to prove the commands reach it through ConversationView.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, screen } from '@testing-library/react';
import { App } from '../components/app.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import {
  setConversations,
  setCommands,
  __resetAppStoreForTests,
} from '../store/app.js';
import {
  ensureSession,
  updateSession,
  __resetSessionsForTests,
} from '../store/sessions.js';

function stubClient() {
  return {
    loadFeedback: vi.fn(async () => []),
    load: vi.fn(async () => ({ turns: [], live: false })),
    createChat: vi.fn(async () => ({ id: 'c-new', title: 'New conversation' })),
    exportConversation: vi.fn(async () => new Blob(['{}'], { type: 'application/json' })),
    postModel: vi.fn(async () => ({})),
    send: vi.fn(() => ({ close: vi.fn() })),
    cancel: vi.fn(async () => ({})),
    feedback: vi.fn(async () => ({})),
  };
}

function renderApp(client = stubClient()) {
  return render(
    <ApiProvider client={client}>
      <App/>
    </ApiProvider>
  );
}

beforeEach(() => {
  window.location.hash = '#/c/c1';
  __resetAppStoreForTests();
  __resetSessionsForTests();
  setConversations([{ id: 'c1', title: 'Chat one', live: false, source: 'http' }]);
  setCommands([{ cmd: '/skill-echo', desc: 'Echo skill', src: 'skill', tool: 'skill_echo' }]);
  ensureSession('c1');
  updateSession('c1', s => ({ ...s, loaded: true }));
});

afterEach(() => {
  cleanup();
  window.location.hash = '';
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

describe('App render', () => {
  it('passes dynamic slash commands into the conversation composer', () => {
    expect(() => renderApp()).not.toThrow();

    const textarea = screen.getByPlaceholderText(/Reply to/i);
    fireEvent.change(textarea, { target: { value: '/skill-echo hello' } });

    const preview = screen.getByTestId('slash-preview');
    expect(preview.textContent).toContain('/skill-echo');
    expect(preview.textContent).toContain('Echo skill');
    expect(preview.textContent).toContain('skill_echo');
  });
});
