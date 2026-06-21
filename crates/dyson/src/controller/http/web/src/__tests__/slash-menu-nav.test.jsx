// Composer slash-menu keyboard navigation.
//
// The slash menu rendered a hard-wired `.focused` highlight on the first
// row but had no keyboard navigation — ArrowUp/Down did nothing and Enter
// sent the message instead of picking the highlighted command.  These
// tests pin the wired-up behaviour: arrows move the highlight (clamped),
// Enter/Tab pick the highlighted command (and do NOT send), and Enter
// still sends normally when the menu is closed.

import React from 'react';
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup } from '@testing-library/react';
import { Composer } from '../components/turns.jsx';

afterEach(() => cleanup());

function openMenu(container, value) {
  const ta = container.querySelector('textarea');
  fireEvent.change(ta, { target: { value } });
  return ta;
}

function focusedCmd(container) {
  const el = container.querySelector('.slashmenu .item.focused .cmd');
  return el ? el.textContent : null;
}

describe('Composer — slash menu keyboard navigation', () => {
  it('opens with the first row highlighted', () => {
    const { container } = render(<Composer onSend={() => {}} onCancel={() => {}} />);
    openMenu(container, '/c'); // → /clear, /compact
    expect(container.querySelector('.slashmenu')).toBeTruthy();
    expect(focusedCmd(container)).toBe('/clear');
  });

  it('ArrowDown / ArrowUp move the highlight and clamp at the ends', () => {
    const { container } = render(<Composer onSend={() => {}} onCancel={() => {}} />);
    const ta = openMenu(container, '/c'); // [/clear, /compact]

    fireEvent.keyDown(ta, { key: 'ArrowDown' });
    expect(focusedCmd(container)).toBe('/compact');
    // Clamp at the bottom — no wrap past the last row.
    fireEvent.keyDown(ta, { key: 'ArrowDown' });
    expect(focusedCmd(container)).toBe('/compact');

    fireEvent.keyDown(ta, { key: 'ArrowUp' });
    expect(focusedCmd(container)).toBe('/clear');
    // Clamp at the top.
    fireEvent.keyDown(ta, { key: 'ArrowUp' });
    expect(focusedCmd(container)).toBe('/clear');
  });

  it('Enter picks the highlighted command instead of sending', () => {
    const onSend = vi.fn();
    const { container } = render(<Composer onSend={onSend} onCancel={() => {}} />);
    const ta = openMenu(container, '/c');

    fireEvent.keyDown(ta, { key: 'ArrowDown' }); // highlight /compact
    fireEvent.keyDown(ta, { key: 'Enter' });

    expect(onSend).not.toHaveBeenCalled();
    expect(container.querySelector('textarea').value).toBe('/compact ');
    expect(container.querySelector('.slashmenu')).toBeNull(); // menu closed
  });

  it('Tab also picks the highlighted command', () => {
    const onSend = vi.fn();
    const { container } = render(<Composer onSend={onSend} onCancel={() => {}} />);
    const ta = openMenu(container, '/c');

    fireEvent.keyDown(ta, { key: 'Tab' });
    expect(onSend).not.toHaveBeenCalled();
    expect(container.querySelector('textarea').value).toBe('/clear ');
  });

  it('the highlight resets to the top as the filter narrows', () => {
    const { container } = render(<Composer onSend={() => {}} onCancel={() => {}} />);
    const ta = openMenu(container, '/c');
    fireEvent.keyDown(ta, { key: 'ArrowDown' }); // /compact
    expect(focusedCmd(container)).toBe('/compact');
    // Typing more characters changes the filter → highlight back to top.
    fireEvent.change(ta, { target: { value: '/co' } }); // still /compact only
    expect(focusedCmd(container)).toBe('/compact');
  });

  it('Enter still sends when the slash menu is not open', () => {
    const onSend = vi.fn();
    const { container } = render(<Composer onSend={onSend} onCancel={() => {}} />);
    const ta = container.querySelector('textarea');
    fireEvent.change(ta, { target: { value: 'hello world' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    expect(onSend).toHaveBeenCalledWith('hello world', []);
  });

  it('clicking a row still inserts that command', () => {
    const { container } = render(<Composer onSend={() => {}} onCancel={() => {}} />);
    openMenu(container, '/mo'); // → /model
    fireEvent.click(container.querySelector('.slashmenu .item'));
    expect(container.querySelector('textarea').value).toBe('/model ');
  });
});
