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

    const textarea = screen.getByRole('textbox', { name: /^Message / });
    fireEvent.change(textarea, { target: { value: '/skill-echo hello' } });

    const preview = screen.getByTestId('slash-preview');
    expect(preview.textContent).toContain('/skill-echo');
    expect(preview.textContent).toContain('Echo skill');
    expect(preview.textContent).toContain('skill_echo');
  });
});

describe('run-state polling (PERF-01)', () => {
  afterEach(() => {
    vi.useRealTimers();
    delete document.hidden;
  });

  it('skips the 2 s run poll while the tab is hidden', async () => {
    vi.useFakeTimers();
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => true });
    const client = {
      ...stubClient(),
      getRun: vi.fn(async () => ({ run_id: 'r1', state: { state: 'completed' }, answer_received: false })),
    };
    renderApp(client);
    await vi.advanceTimersByTimeAsync(10_000);
    // The mount-time refresh may run once; the interval must not.
    expect(client.getRun.mock.calls.length).toBeLessThanOrEqual(1);
  });
});

describe('export failure (Phase 4: no native dialogs)', () => {
  it('reports a failed export in an in-app dialog, not window.alert', async () => {
    const alertSpy = vi.spyOn(window, 'alert').mockImplementation(() => {});
    const client = {
      ...stubClient(),
      exportConversation: vi.fn(async () => { throw new Error('disk on fire'); }),
    };
    renderApp(client);
    fireEvent.click(screen.getByLabelText('Download ShareGPT export'));
    const dialog = await screen.findByRole('dialog');
    expect(dialog.textContent).toContain('disk on fire');
    expect(alertSpy).not.toHaveBeenCalled();
    alertSpy.mockRestore();
  });
});
