// LeftRail "Filter conversations" input.
//
// The input element existed for ages with no state, no onChange, and no
// connection to the rendered list — typing did nothing.  These tests
// mount LeftRail under jsdom + @testing-library/react and verify the
// input drives the visible conversation rows.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, cleanup } from '@testing-library/react';
import { LeftRail } from '../components/views.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { setConversations, __resetAppStoreForTests } from '../store/app.js';
import { __resetSessionsForTests } from '../store/sessions.js';

function stubClient() {
  return {
    createChat: async () => ({ id: 'new', title: 'new' }),
    deleteChat: async () => ({}),
  };
}

function renderWithApi(ui, client = stubClient()) {
  return render(<ApiProvider client={client}>{ui}</ApiProvider>);
}

beforeEach(() => {
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

afterEach(() => {
  cleanup();
});

function rowTitles(container) {
  return [...container.querySelectorAll('.conv .title')].map(el => el.textContent);
}

describe('LeftRail — filter conversations', () => {
  it('typing in the filter input narrows the list to matching titles', () => {
    setConversations([
      { id: 'c1', title: 'Alpha review',   live: false, source: 'http' },
      { id: 'c2', title: 'Beta planning',  live: false, source: 'http' },
      { id: 'c3', title: 'Gamma research', live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    expect(rowTitles(container)).toEqual(['Alpha review', 'Beta planning', 'Gamma research']);

    const input = container.querySelector('.search input');
    expect(input, 'filter input must exist').toBeTruthy();

    fireEvent.change(input, { target: { value: 'beta' } });
    expect(rowTitles(container), 'filter must narrow rows by title substring').toEqual(['Beta planning']);
  });

  it('filter is case-insensitive', () => {
    setConversations([
      { id: 'c1', title: 'Alpha review',   live: false, source: 'http' },
      { id: 'c2', title: 'beta PLANNING',  live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    const input = container.querySelector('.search input');
    fireEvent.change(input, { target: { value: 'BETA' } });
    expect(rowTitles(container)).toEqual(['beta PLANNING']);
  });

  it('filter matches conversation id (mono row exposes the id)', () => {
    setConversations([
      { id: 'abc-123', title: 'First',  live: false, source: 'http' },
      { id: 'xyz-789', title: 'Second', live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    const input = container.querySelector('.search input');
    fireEvent.change(input, { target: { value: 'xyz' } });
    expect(rowTitles(container)).toEqual(['Second']);
  });

  it('clearing the input restores the full list', () => {
    setConversations([
      { id: 'c1', title: 'Alpha',   live: false, source: 'http' },
      { id: 'c2', title: 'Beta',    live: false, source: 'http' },
      { id: 'c3', title: 'Gamma',   live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    const input = container.querySelector('.search input');
    fireEvent.change(input, { target: { value: 'alp' } });
    expect(rowTitles(container)).toEqual(['Alpha']);
    fireEvent.change(input, { target: { value: '' } });
    expect(rowTitles(container)).toEqual(['Alpha', 'Beta', 'Gamma']);
  });

  it('input is a controlled element — its value reflects what was typed', () => {
    setConversations([
      { id: 'c1', title: 'Alpha', live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    const input = container.querySelector('.search input');
    fireEvent.change(input, { target: { value: 'hello' } });
    expect(input.value, 'controlled input must surface typed value').toBe('hello');
  });

  it('non-matching filter shows the empty state and a count of 0', () => {
    setConversations([
      { id: 'c1', title: 'Alpha', live: false, source: 'http' },
      { id: 'c2', title: 'Beta',  live: false, source: 'http' },
    ]);
    const { container } = renderWithApi(
      <LeftRail active={null} setActive={() => {}}/>
    );
    const input = container.querySelector('.search input');
    fireEvent.change(input, { target: { value: 'zzz-no-match' } });
    expect(rowTitles(container)).toEqual([]);
    // Empty-state copy reuses the existing "no conversations yet" branch.
    expect(container.textContent).toMatch(/No conversations/i);
  });
});
