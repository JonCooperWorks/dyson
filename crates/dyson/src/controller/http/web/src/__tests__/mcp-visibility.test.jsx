import React from 'react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';

import { EmptyState } from '../components/turns.jsx';
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
});
