import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { EmptyState, Composer } from '../components/turns.jsx';
import { RunControls } from '../components/run-controls.jsx';
import { LeftRail } from '../components/views.jsx';
import { ApiProvider } from '../hooks/useApi.js';
import { __resetAppStoreForTests, setConversations } from '../store/app.js';

beforeEach(__resetAppStoreForTests);
afterEach(cleanup);

describe('agent workspace', () => {
  it('offers editable starting prompts without sending a turn', () => {
    const select = vi.fn();
    render(<EmptyState onSelectPrompt={select}/>);
    fireEvent.click(screen.getByRole('button', { name: /Explore a question/ }));
    expect(select).toHaveBeenCalledWith(expect.stringContaining('Help me research a question'));
    expect(screen.getAllByRole('button')).toHaveLength(3);
  });

  it('keeps a draft when a saved run blocks sending', () => {
    const send = vi.fn();
    const draft = vi.fn();
    render(<Composer blocked onSend={send} draftText="Keep this for later" onDraftChange={draft}/>);
    expect(screen.getByRole('button', { name: 'Send message' }).disabled).toBe(true);
    fireEvent.keyDown(screen.getByRole('textbox', { name: /^Message / }), { key: 'Enter' });
    expect(send).not.toHaveBeenCalled();
    expect(draft).not.toHaveBeenCalled();
    expect(screen.getByRole('textbox', { name: /^Message / }).value).toBe('Keep this for later');
  });

  it('provides named conversation buttons and a useful empty search', () => {
    const open = vi.fn();
    setConversations([{ id: 'private-id', title: 'Release plan', live: false }]);
    render(<ApiProvider client={{}}><LeftRail active="private-id" setActive={open}/></ApiProvider>);
    const row = screen.getByRole('button', { name: 'Open conversation: Release plan' });
    expect(row.getAttribute('aria-current')).toBe('page');
    fireEvent.click(row);
    expect(open).toHaveBeenCalledWith('private-id');
    expect(screen.queryByText('private-id')).toBeNull();
    fireEvent.change(screen.getByRole('textbox', { name: 'Search conversations' }), { target: { value: 'missing' } });
    expect(screen.getByText('No matching conversations')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Clear search' }));
    expect(screen.getByRole('button', { name: 'Open conversation: Release plan' })).toBeTruthy();
  });
});

describe('saved work controls', () => {
  it('resumes the saved run and prevents repeated actions while dispatching', () => {
    const action = vi.fn();
    const run = { state: { state: 'paused' } };
    const { rerender } = render(<RunControls run={run} onAction={action}/>);
    fireEvent.click(screen.getByRole('button', { name: 'Resume' }));
    expect(action).toHaveBeenCalledWith('resume');
    rerender(<RunControls run={run} pending="resume" onAction={action}/>);
    expect(screen.getByRole('button', { name: 'Resuming…' }).disabled).toBe(true);
    expect(screen.getByRole('button', { name: 'Cancel run' }).disabled).toBe(true);
  });

  it('requires an answer before offering resume', () => {
    const run = { state: { state: 'waiting_for_input', request: { question: 'Which branch?' } } };
    const { rerender } = render(<RunControls run={run}/>);
    expect(screen.getByText('Which branch?')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Resume' })).toBeNull();
    rerender(<RunControls run={{ ...run, answer_received: true }}/>);
    expect(screen.getByRole('button', { name: 'Resume' })).toBeTruthy();
  });

  it('acknowledges cooperative pause and hides completed controls', () => {
    const { rerender } = render(<RunControls run={{ state: { state: 'running' } }} running pending="pause"/>);
    expect(screen.getByText('Pausing after the current step')).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Pausing…' }).disabled).toBe(true);
    rerender(<RunControls run={{ state: { state: 'finished' } }}/>);
    expect(screen.queryByRole('region', { name: 'Run controls' })).toBeNull();
  });
});
