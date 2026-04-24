import React from 'react';
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { render, cleanup, act } from '@testing-library/react';
import { useSession, useSessionMutator } from './useSession.js';
import { ensureSession, updateSession, __resetSessionsForTests } from '../store/sessions.js';

beforeEach(() => { __resetSessionsForTests(); });
afterEach(() => cleanup());

describe('useSession', () => {
  it('returns the frozen session snapshot for the chatId', () => {
    ensureSession('c1');
    updateSession('c1', s => ({ ...s, running: true }));
    function Probe() {
      const s = useSession('c1');
      return <div data-testid="out">{String(s && s.running)}</div>;
    }
    const { getByTestId } = render(<Probe/>);
    expect(getByTestId('out').textContent).toBe('true');
  });

  it('re-renders only when the chat\'s own slice changes', () => {
    let renders = 0;
    function Probe() {
      renders += 1;
      const s = useSession('c1');
      return <div>{s ? s.liveTurns.length : -1}</div>;
    }
    render(<Probe/>);
    const base = renders;
    // Touch a sibling chat — should NOT re-render our probe.
    act(() => { updateSession('c2', s => ({ ...s, running: true })); });
    expect(renders).toBe(base);
    // Touch our chat — SHOULD re-render.
    act(() => { updateSession('c1', s => ({ ...s, running: true })); });
    expect(renders).toBeGreaterThan(base);
  });

  it('null chatId yields null and re-renders only when the id fills in', () => {
    function Probe({ id }) {
      const s = useSession(id);
      return <div data-testid="out">{s === null ? 'null' : 'session'}</div>;
    }
    const { getByTestId, rerender } = render(<Probe id={null}/>);
    expect(getByTestId('out').textContent).toBe('null');
    act(() => { updateSession('c1', s => ({ ...s, running: true })); });
    rerender(<Probe id="c1"/>);
    expect(getByTestId('out').textContent).toBe('session');
  });

  it('two chats render into isolated components without cross-talk', () => {
    updateSession('c1', s => ({ ...s, tname: 'bash' }));
    updateSession('c2', s => ({ ...s, tname: 'diff' }));
    function Probe({ id }) {
      const s = useSession(id);
      return <div data-testid={`out-${id}`}>{s ? s.tname : ''}</div>;
    }
    const { getByTestId } = render(
      <div>
        <Probe id="c1"/>
        <Probe id="c2"/>
      </div>
    );
    expect(getByTestId('out-c1').textContent).toBe('bash');
    expect(getByTestId('out-c2').textContent).toBe('diff');
    act(() => { updateSession('c1', s => ({ ...s, tname: 'rg' })); });
    expect(getByTestId('out-c1').textContent).toBe('rg');
    // Sibling chat unchanged.
    expect(getByTestId('out-c2').textContent).toBe('diff');
  });
});

describe('useSessionMutator', () => {
  it('returns a dispatcher bound to the chat id', () => {
    function Probe() {
      const mutate = useSessionMutator('c1');
      React.useEffect(() => {
        mutate(s => ({ ...s, running: true }));
      }, [mutate]);
      const s = useSession('c1');
      return <div data-testid="out">{String(s && s.running)}</div>;
    }
    const { getByTestId } = render(<Probe/>);
    expect(getByTestId('out').textContent).toBe('true');
  });
});
