import React from 'react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, cleanup, act } from '@testing-library/react';
import { useAppState } from './useAppState.js';
import {
  setConversations,
  setActivity,
  __resetAppStoreForTests,
} from '../store/app.js';

beforeEach(() => { __resetAppStoreForTests(); });

describe('useAppState', () => {
  afterEach(() => cleanup());

  it('renders the selected slice', () => {
    setConversations([{ id: 'a', title: 'Alpha' }]);
    function Probe() {
      const convs = useAppState(s => s.conversations);
      return <div data-testid="out">{convs.map(c => c.title).join(',')}</div>;
    }
    const { getByTestId } = render(<Probe/>);
    expect(getByTestId('out').textContent).toBe('Alpha');
  });

  it('re-renders when the selected slice changes', () => {
    let renders = 0;
    function Probe() {
      renders += 1;
      const convs = useAppState(s => s.conversations);
      return <div>{convs.length}</div>;
    }
    render(<Probe/>);
    const base = renders;
    act(() => {
      setConversations([{ id: 'a' }]);
    });
    expect(renders).toBeGreaterThan(base);
  });

  it('does NOT re-render when an unrelated slice changes', () => {
    setConversations([{ id: 'a' }]);
    let renders = 0;
    function Probe() {
      renders += 1;
      // Only subscribe to conversations.
      const convs = useAppState(s => s.conversations);
      return <div>{convs.length}</div>;
    }
    render(<Probe/>);
    const base = renders;
    act(() => {
      setActivity([{ name: 'z', status: 'running' }]);
    });
    // Activity changed, conversations did not — our probe must not re-render.
    expect(renders).toBe(base);
  });

  it('returns the whole snapshot when no selector is supplied', () => {
    function Probe() {
      const s = useAppState();
      return <div data-testid="out">{String(s.live)}</div>;
    }
    const { getByTestId } = render(<Probe/>);
    expect(getByTestId('out').textContent).toBe('false');
  });
});
