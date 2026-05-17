import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';

import { TopBar } from '../components/views.jsx';
import { EmptyState } from '../components/turns.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { __resetAppStoreForTests, setAgentInfo, setProviders } from '../store/app.js';

beforeEach(() => {
  __resetAppStoreForTests();
});

afterEach(() => {
  cleanup();
});

describe('MCP visibility', () => {
  it('shows configured MCP server names in the empty chat state', () => {
    setProviders([{ id: 'p', active: true, activeModel: 'm', models: ['m'] }], 'deepseek/v4');
    setAgentInfo({
      name: 'axelrod',
      skills: {
        mcp: [
          { name: 'mcp_massive', transport: 'http' },
          { name: 'brave-search', transport: 'http' },
        ],
      },
    });

    render(<EmptyState/>);

    expect(screen.getByText(/mcp_massive/)).toBeTruthy();
    expect(screen.getByText(/brave-search/)).toBeTruthy();
  });

  it('does not show the old header MCP count badge', () => {
    setProviders([{ id: 'p', active: true, activeModel: 'm', models: ['m'] }], 'deepseek/v4');
    setAgentInfo({
      name: 'axelrod',
      skills: {
        mcp: [
          { name: 'mcp_massive', transport: 'http' },
          { name: 'brave-search', transport: 'http' },
          { name: 'agentmail', transport: 'http' },
        ],
      },
    });

    const { container } = render(
      <ApiProvider client={{ postModel: async () => ({}) }}>
        <TopBar view="conv" setView={() => {}} onToggleLeft={() => {}}/>
      </ApiProvider>
    );

    expect(container.querySelector('.mcp-count')).toBeNull();
    expect(screen.getByText('deepseek/v4')).toBeTruthy();
  });
});

describe('TopBar model steering', () => {
  it('idle model pick posts the current model switch', async () => {
    setProviders([{ id: 'p', name: 'Provider', active: true, activeModel: 'm1', models: ['m1', 'm2'] }], 'm1');
    const postModel = vi.fn(async () => ({}));
    render(
      <ApiProvider client={{ postModel }}>
        <TopBar view="conv" setView={() => {}} onToggleLeft={() => {}}/>
      </ApiProvider>
    );

    fireEvent.click(screen.getByTitle('Switch model'));
    fireEvent.click(screen.getByText('m2'));
    await vi.waitFor(() => expect(postModel).toHaveBeenCalledWith('p', 'm2'));
  });

  it('running model pick is surfaced as next-run steering', async () => {
    setProviders([{ id: 'p', name: 'Provider', active: true, activeModel: 'm1', models: ['m1', 'm2'] }], 'm1');
    const onPickModel = vi.fn(async () => {});
    render(
      <ApiProvider client={{ postModel: async () => ({}) }}>
        <TopBar
          view="conv"
          setView={() => {}}
          onToggleLeft={() => {}}
          running={true}
          nextRunModel={{ provider: 'p', model: 'm2' }}
          onPickModel={onPickModel}/>
      </ApiProvider>
    );

    expect(screen.getByText('next')).toBeTruthy();
    expect(screen.getByText('m2')).toBeTruthy();
    fireEvent.click(screen.getByTitle('Switch model'));
    fireEvent.click(screen.getByText('m1'));
    await vi.waitFor(() => expect(onPickModel).toHaveBeenCalledWith('p', 'm1'));
  });
});
