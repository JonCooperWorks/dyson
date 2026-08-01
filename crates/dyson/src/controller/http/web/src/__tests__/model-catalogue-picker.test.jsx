// The in-UI model switcher is a searchable picker over the FULL catalogue
// (GET /api/models), not just the models named in dyson.json — a managed
// dyson now seeds a single model, so the old provider-tree menu had nothing
// to switch to.  These pin: lazy catalogue fetch on open, type-to-filter,
// picking a catalogue model, and graceful degrade when no catalogue is
// reachable.

import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';

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
  provider: 'openrouter',
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

    // Picking a catalogue model posts the switch for its owning Swarm route,
    // independent of whichever execution backend is currently active.
    fireEvent.click(screen.getByText('openai/gpt-5'));
    await waitFor(() => expect(postModel).toHaveBeenCalledWith('openrouter', 'openai/gpt-5'));
  });

  it('shows every catalogue model instead of truncating the swarm inventory', async () => {
    setProviders([{ id: 'openrouter', name: 'Swarm', active: true, activeModel: 'seed', models: ['seed'] }], 'seed');
    const models = Array.from({ length: 75 }, (_, i) => ({ id: `vendor/model-${i}` }));
    renderTopBar({
      listModels: vi.fn(async () => ({ provider: 'openrouter', models })),
      postModel: vi.fn(async () => ({})),
    });

    fireEvent.click(screen.getByTitle('Switch model'));
    expect(await screen.findByText('Catalogue · 75')).toBeTruthy();
    expect(screen.getByText('vendor/model-74')).toBeTruthy();
    expect(screen.queryByText(/more.*narrow/i)).toBeNull();
  });

  it('routes a Swarm catalogue pick correctly while Codex is active', async () => {
    setProviders([{
      id: 'chatgpt-subscription', name: 'ChatGPT subscription', backend: 'codex', active: true,
      activeModel: 'gpt-5.6-sol', models: ['gpt-5.6-sol'],
    }, {
      id: 'openrouter', name: 'Swarm', backend: 'openai', active: false,
      activeModel: 'seed', models: ['seed'],
    }], 'gpt-5.6-sol');
    const postModel = vi.fn(async () => ({}));
    renderTopBar({ listModels: vi.fn(async () => CATALOGUE), postModel });

    fireEvent.click(screen.getByTitle('Switch model'));
    fireEvent.click(await screen.findByText('openai/gpt-5'));
    await waitFor(() => expect(postModel).toHaveBeenCalledWith('openrouter', 'openai/gpt-5'));
  });

  it('keeps configured models searchable (they are excluded from the catalogue list)', async () => {
    // Two configured models; the catalogue also carries deepseek-v4-pro, so
    // it must be de-duped out of the catalogue list — but a search for it
    // must still surface it from the "current" group, not vanish.
    setProviders([{
      id: 'p', name: 'Provider', active: true, activeModel: 'deepseek/deepseek-v4-pro',
      models: ['deepseek/deepseek-v4-pro', 'moonshotai/kimi-k3'],
    }], 'deepseek/deepseek-v4-pro');
    const listModels = vi.fn(async () => ({
      models: [
        { id: 'deepseek/deepseek-v4-pro', name: 'DeepSeek V4 Pro' },
        { id: 'openai/gpt-5', name: 'GPT-5' },
      ],
    }));
    const postModel = vi.fn(async () => ({}));
    renderTopBar({ listModels, postModel });

    fireEvent.click(screen.getByTitle('Switch model'));
    const search = await screen.findByLabelText('Search models');
    fireEvent.change(search, { target: { value: 'deepseek-v4-pro' } });

    // Found via the configured group, with no misleading empty state.
    const menu = document.querySelector('.modelmenu');
    await waitFor(() => expect(within(menu).getByText('deepseek/deepseek-v4-pro')).toBeTruthy());
    expect(screen.queryByText('No matching models.')).toBeNull();
    fireEvent.click(menu.querySelector('.item .model'));
    await waitFor(() => expect(postModel).toHaveBeenCalledWith('p', 'deepseek/deepseek-v4-pro'));
  });

  it('keeps an unavailable catalogue quiet when configured models remain usable', async () => {
    setProviders([{ id: 'p', name: 'Provider', active: true, activeModel: 'seed', models: ['seed'] }], 'seed');
    const listModels = vi.fn(async () => ({ models: [] }));
    renderTopBar({ listModels, postModel: vi.fn(async () => ({})) });

    fireEvent.click(screen.getByTitle('Switch model'));
    await waitFor(() => expect(listModels).toHaveBeenCalled());
    // No throw or implementation-noise empty state; the seeded model remains
    // pickable from the configured group.
    await waitFor(() => expect(screen.queryByText('Catalogue · loading')).toBeNull());
    expect(screen.queryByText('No catalogue available.')).toBeNull();
    // 'seed' shows both in the top-bar button and as a pickable item in the
    // "current" group.
    expect(screen.getAllByText('seed').length).toBeGreaterThanOrEqual(2);
  });

  it('starts device auth before selecting the ChatGPT subscription provider', async () => {
    setProviders([{
      id: 'chatgpt-subscription', name: 'ChatGPT subscription', backend: 'codex', active: false,
      activeModel: 'gpt-5.6-sol', models: ['gpt-5.6-sol'],
    }, {
      id: 'openrouter', name: 'Swarm', backend: 'openai', active: true, activeModel: 'seed', models: ['seed'],
    }], 'seed');
    const client = {
      listModels: vi.fn(async () => ({ models: [] })),
      postModel: vi.fn(async () => ({})),
      getCodexAuth: vi.fn(async () => ({ connected: false, state: 'pending', verification_uri: 'https://auth.openai.com/codex/device', user_code: 'ABCD-12345' })),
      startCodexAuth: vi.fn(async () => ({ connected: false, state: 'pending', verification_uri: 'https://auth.openai.com/codex/device', user_code: 'ABCD-12345' })),
    };
    renderTopBar(client);

    fireEvent.click(screen.getByTitle('Switch model'));
    const menu = document.querySelector('.modelmenu');
    fireEvent.click(within(menu).getByText('gpt-5.6-sol'));

    await waitFor(() => expect(client.startCodexAuth).toHaveBeenCalledTimes(1));
    expect(client.postModel).not.toHaveBeenCalled();
    expect(await screen.findByText('ABCD-12345')).toBeTruthy();
    expect(screen.getByText('Open ChatGPT sign-in').getAttribute('href'))
      .toBe('https://auth.openai.com/codex/device');
  });

  it('marks Codex elegantly while keeping the default Swarm route visually quiet', async () => {
    setProviders([{
      id: 'chatgpt-subscription', name: 'chatgpt-subscription', backend: 'codex', active: true,
      activeModel: 'gpt-5.6-sol', models: ['gpt-5.6-sol', 'gpt-5.6-terra'],
    }, {
      id: 'openrouter', name: 'openrouter', backend: 'openai', active: false,
      activeModel: 'deepseek/deepseek-v4-pro', models: ['deepseek/deepseek-v4-pro'],
    }], 'gpt-5.6-sol');
    renderTopBar({ listModels: vi.fn(async () => ({ models: [] })), postModel: vi.fn(async () => ({})) });

    const switcher = screen.getByTitle('Switch model');
    expect(within(switcher).getByText('Codex')).toBeTruthy();
    expect(switcher.getAttribute('aria-label')).toContain('Execution backend Codex, ChatGPT subscription');

    fireEvent.click(switcher);
    const menu = document.querySelector('.modelmenu');
    expect(within(menu).queryByLabelText('Active execution backend')).toBeNull();
    expect(within(menu).getAllByText('Codex')).toHaveLength(2);
    expect(within(menu).queryByText('Swarm')).toBeNull();
    expect(within(menu).queryByText('ChatGPT subscription')).toBeNull();
    expect(within(menu).queryByText('No catalogue available.')).toBeNull();
  });

  it('shows no provider badge when Swarm is active', () => {
    setProviders([{
      id: 'openrouter', name: 'openrouter', backend: 'openai', active: true,
      activeModel: 'deepseek/deepseek-v4-pro', models: ['deepseek/deepseek-v4-pro'],
    }], 'deepseek/deepseek-v4-pro');
    renderTopBar({ listModels: vi.fn(async () => ({ models: [] })), postModel: vi.fn(async () => ({})) });

    const switcher = screen.getByTitle('Switch model');
    expect(within(switcher).queryByText('Swarm')).toBeNull();
    expect(within(switcher).queryByText('Codex')).toBeNull();
    expect(switcher.getAttribute('aria-label')).toContain('Execution backend Swarm');
  });
});
