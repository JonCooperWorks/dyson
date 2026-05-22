import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { CommandPalette, buildCommandItems } from '../components/command-palette.jsx';

afterEach(() => cleanup());

describe('CommandPalette', () => {
  const props = () => ({
    open: true,
    onClose: vi.fn(),
    conversations: [{ id: 'c-1', title: 'Incident review' }],
    providers: [{ id: 'anthropic', name: 'Anthropic', models: ['claude-opus'] }],
    mindFiles: [{ path: 'williamwoodward-financial-analysis.md', size: '120 B' }],
    onSelectConversation: vi.fn(),
    onSelectView: vi.fn(),
    onSelectModel: vi.fn(),
    onOpenMindFile: vi.fn(),
    onInsertSlash: vi.fn(),
  });

  it('builds navigation, conversation, model, mind, and slash command items', () => {
    const items = buildCommandItems(props());
    expect(items.some(i => i.id === 'view:mind')).toBe(true);
    expect(items.some(i => i.id === 'conversation:c-1')).toBe(true);
    expect(items.some(i => i.id === 'model:anthropic:claude-opus')).toBe(true);
    expect(items.some(i => i.id === 'mind:williamwoodward-financial-analysis.md')).toBe(true);
    expect(items.some(i => i.id === 'slash:/clear')).toBe(true);
  });

  it('uses dynamic slash commands when provided', () => {
    const p = props();
    const items = buildCommandItems({
      ...p,
      slashCommands: [{ cmd: '/skill-echo', desc: 'Echo skill', src: 'skill' }],
    });
    expect(items.some(i => i.id === 'slash:/skill-echo')).toBe(true);
    expect(items.some(i => i.id === 'slash:/clear')).toBe(false);
  });

  it('filters and selects an item with Enter', () => {
    const p = props();
    render(<CommandPalette {...p}/>);
    const input = screen.getByLabelText('Command palette search');
    fireEvent.change(input, { target: { value: 'incident' } });
    expect(screen.getByText('Incident review')).toBeTruthy();
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(p.onSelectConversation).toHaveBeenCalledWith('c-1');
    expect(p.onClose).toHaveBeenCalled();
  });

  it('supports arrow key focus and Escape close', () => {
    const p = props();
    render(<CommandPalette {...p}/>);
    const input = screen.getByLabelText('Command palette search');
    fireEvent.keyDown(input, { key: 'ArrowDown' });
    fireEvent.keyDown(input, { key: 'ArrowUp' });
    fireEvent.keyDown(input, { key: 'Escape' });
    expect(p.onClose).toHaveBeenCalled();
  });
});
