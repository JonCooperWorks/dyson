// The in-UI model switcher is a searchable picker over the FULL catalogue
// (GET /api/models), not just the models named in dyson.json — a managed
// dyson now seeds a single model, so the old provider-tree menu had nothing
// to switch to.  These pin: lazy catalogue fetch on open, type-to-filter,
// picking a catalogue model, and graceful degrade when no catalogue is
// reachable.

import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

import { TopBar } from '../components/views.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { __resetAppStoreForTests, setProviders } from '../store/app.js';

beforeEach(() => {
  __resetAppStoreForTests();
});

afterEach(() => {
  cleanup();
});

const CATALOGUE = {
  models: [
    { id: 'anthropic/claude-opus-4', name: 'Claude Opus 4', context_length: 200000 },
    { id: 'deepseek/deepseek-v4-pro', name: 'DeepSeek V4 Pro', context_length: 128000 },
    { id: 'openai/gpt-5', name: 'GPT-5', context_length: 400000 },
  ],
};

function renderTopBar(client) {
  return render(
    <ApiProvider client={client}>
      <TopBar view="conv" setView={() => {}} onToggleLeft={() => {}}/>
    </ApiProvider>
  );
}

describe('TopBar — catalogue picker', () => {
  it('lazily fetches the catalogue on open and switches to a picked model', async () => {
    // One seeded model configured — the picker must reach beyond it.
    setProviders([{ id: 'p', name: 'Provider', active: true, activeModel: 'seed', models: ['seed'] }], 'seed');
    const listModels = vi.fn(async () => CATALOGUE);
    const postModel = vi.fn(async () => ({}));
    renderTopBar({ listModels, postModel });

    // Enabled despite only one configured model.
    const btn = screen.getByTitle('Switch model');
    expect(listModels).not.toHaveBeenCalled();
    fireEvent.click(btn);

    // Catalogue fetched exactly once on open.
    await waitFor(() => expect(listModels).toHaveBeenCalledTimes(1));
    // Search box + catalogue entries render.
    const search = await screen.findByLabelText('Search models');
    await screen.findByText('openai/gpt-5');

    // Filter narrows the list.
    fireEvent.change(search, { target: { value: 'gpt' } });
    await waitFor(() => expect(screen.queryByText('anthropic/claude-opus-4')).toBeNull());
    expect(screen.getByText('openai/gpt-5')).toBeTruthy();

    // Picking a catalogue model posts the switch for the active provider.
    fireEvent.click(screen.getByText('openai/gpt-5'));
    await waitFor(() => expect(postModel).toHaveBeenCalledWith('p', 'openai/gpt-5'));
  });

  it('degrades to a graceful empty state when no catalogue is reachable', async () => {
    setProviders([{ id: 'p', name: 'Provider', active: true, activeModel: 'seed', models: ['seed'] }], 'seed');
    const listModels = vi.fn(async () => ({ models: [] }));
    renderTopBar({ listModels, postModel: vi.fn(async () => ({})) });

    fireEvent.click(screen.getByTitle('Switch model'));
    await waitFor(() => expect(listModels).toHaveBeenCalled());
    // No throw; the empty-catalogue hint shows and the seeded model is still
    // pickable from the "current" group.
    expect(await screen.findByText('No catalogue available.')).toBeTruthy();
    // 'seed' shows both in the top-bar button and as a pickable item in the
    // "current" group.
    expect(screen.getAllByText('seed').length).toBeGreaterThanOrEqual(2);
  });
});
