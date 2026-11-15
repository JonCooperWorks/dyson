// The way back into Swarm (UX spec 2.4): the Swarm bar in the header, the
// Audit view's "See all activity in Swarm" and "Add credit in Swarm" on a
// balance error. All three are drawn only when the Swarm proxy answers
// `GET /_swarm/context`; a self-hosted Dyson shows none of them.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { render, cleanup, waitFor } from '@testing-library/react';
import { getSwarmContext } from 'dyson-common-ui';
import { App } from '../components/app.jsx';
import { ErrorBlock, isBalanceError } from '../components/turns.jsx';
import { AuditView } from '../components/views-secondary.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { __resetAppStoreForTests } from '../store/app.js';
import { __resetSessionsForTests } from '../store/sessions.js';

const CONTEXT = {
  apex_url: 'https://swarm.example.com/',
  instance: { id: 'tars-1', name: 'TARS' },
  agents: [
    { id: 'case-2', name: 'Case', open_url: 'https://case-2.swarm.example.com/' },
    { id: 'tars-1', name: 'TARS', open_url: 'https://tars-1.swarm.example.com/' },
  ],
};

function swarmAnswers(body) {
  const fetch = vi.fn(async () => ({
    ok: true, headers: { get: () => 'application/json' }, json: async () => body,
  }));
  getSwarmContext({ fetch, reset: true });
  return fetch;
}

function selfHosted() {
  getSwarmContext({ fetch: async () => ({ ok: false, headers: { get: () => 'text/html' } }), reset: true });
}

function stubClient(over = {}) {
  return {
    loadFeedback: vi.fn(async () => []),
    load: vi.fn(async () => ({ messages: [], live: false })),
    createChat: vi.fn(async () => ({ id: 'c-new', title: 'New conversation' })),
    postModel: vi.fn(async () => ({})),
    send: vi.fn(() => ({ close: vi.fn() })),
    cancel: vi.fn(async () => ({})),
    feedback: vi.fn(async () => ({})),
    getAudit: vi.fn(async () => ({ requests: [], source: 'swarm' })),
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
});

describe('Swarm bar', () => {
  it('sits left of the Dyson mark and links back to the agent in Swarm', async () => {
    swarmAnswers(CONTEXT);
    const { container } = render(<ApiProvider client={stubClient()}><App/></ApiProvider>);
    await waitFor(() => expect(container.querySelector('.topbar .swarm-bar')).toBeTruthy());
    const bar = container.querySelector('.topbar .swarm-bar');
    expect(bar.nextElementSibling.classList.contains('brand')).toBe(true);
    expect(bar.querySelector('a').getAttribute('href')).toBe('https://swarm.example.com/#/i/tars-1');
  });

  it('is absent on a self-hosted Dyson', async () => {
    selfHosted();
    const { container } = render(<ApiProvider client={stubClient()}><App/></ApiProvider>);
    await new Promise(r => setTimeout(r, 0));
    expect(container.querySelector('.swarm-bar')).toBeNull();
  });
});

describe('contextual Swarm links', () => {
  it('Audit links to the agent activity in Swarm', async () => {
    swarmAnswers(CONTEXT);
    const { findByText } = render(<ApiProvider client={stubClient()}><AuditView/></ApiProvider>);
    const link = await findByText('See all activity in Swarm');
    expect(link.getAttribute('href')).toBe('https://swarm.example.com/#/i/tars-1/activity');
  });

  it('a balance error offers Add credit in Swarm, built from the context not the message', async () => {
    swarmAnswers(CONTEXT);
    const message = "You're out of credit. Add credit to keep going: https://evil.example.net/#/cost";
    const { findByText } = render(<ErrorBlock block={{ type: 'error', message }}/>);
    const link = await findByText('Add credit in Swarm');
    expect(link.getAttribute('href')).toBe('https://swarm.example.com/#/settings/billing');
  });

  it('other errors and self-hosted Dysons get no credit link', async () => {
    swarmAnswers(CONTEXT);
    const other = render(<ErrorBlock block={{ type: 'error', message: 'Anthropic (529): Overloaded' }}/>);
    await new Promise(r => setTimeout(r, 0));
    expect(other.queryByText('Add credit in Swarm')).toBeNull();
    cleanup();
    selfHosted();
    const mine = render(<ErrorBlock block={{ type: 'error', message: 'OpenRouter (402 Payment Required)' }}/>);
    await new Promise(r => setTimeout(r, 0));
    expect(mine.queryByText('Add credit in Swarm')).toBeNull();
    expect(isBalanceError('OpenRouter (402 Payment Required)')).toBe(true);
    expect(isBalanceError('Anthropic (529): Overloaded')).toBe(false);
  });
});
