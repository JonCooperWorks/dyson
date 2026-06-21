/* Tests for the reusable searchable <Combobox>. */
import { describe, expect, test, afterEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, cleanup, within } from '@testing-library/react';
import '@testing-library/jest-dom/vitest';

import { Combobox } from './Combobox.jsx';

afterEach(cleanup);

const OPTIONS = [
  { value: 'a', label: 'Apple' },
  { value: 'b', label: 'Banana' },
  { value: 'c', label: 'Cherry' },
];

const list = () => document.querySelector('.combobox-list');
const options = () => within(list()).getAllByRole('option').map(o => o.textContent);

function renderBox(overrides = {}) {
  const props = {
    options: OPTIONS,
    value: '',
    onSelect: () => {},
    ariaLabel: 'fruit',
    ...overrides,
  };
  render(React.createElement(Combobox, props));
  return screen.getByLabelText('fruit');
}

describe('Combobox', () => {
  test('focus opens the list with every option', () => {
    const input = renderBox();
    expect(list()).toBeNull();
    fireEvent.focus(input);
    expect(options()).toEqual(['Apple', 'Banana', 'Cherry']);
  });

  test('typing filters; clearing restores the full list', () => {
    const input = renderBox();
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: 'an' } });
    expect(options()).toEqual(['Banana']); // "an" matches banAna
    fireEvent.change(input, { target: { value: '' } });
    expect(options()).toEqual(['Apple', 'Banana', 'Cherry']);
  });

  test('a committed value still reveals all options on focus (no self-filtering)', () => {
    const input = renderBox({ value: 'b' });
    expect(input).toHaveValue('Banana');
    fireEvent.focus(input);
    expect(options()).toEqual(['Apple', 'Banana', 'Cherry']);
  });

  test('clicking an option selects it', () => {
    let picked = null;
    const input = renderBox({ onSelect: (o) => { picked = o; } });
    fireEvent.focus(input);
    fireEvent.mouseDown(within(list()).getByText('Cherry'));
    expect(picked).toEqual({ value: 'c', label: 'Cherry' });
    expect(list()).toBeNull(); // closes after select
  });

  test('arrow keys + Enter select the highlighted option', () => {
    let picked = null;
    const input = renderBox({ onSelect: (o) => { picked = o; } });
    fireEvent.focus(input);
    fireEvent.keyDown(input, { key: 'ArrowDown' }); // Apple
    fireEvent.keyDown(input, { key: 'ArrowDown' }); // Banana
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(picked).toEqual({ value: 'b', label: 'Banana' });
  });

  test('Enter on an exact typed name selects it', () => {
    let picked = null;
    const input = renderBox({ onSelect: (o) => { picked = o; } });
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: 'cherry' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(picked).toEqual({ value: 'c', label: 'Cherry' });
  });

  test('emptying the box fires onClear', () => {
    let cleared = false;
    const input = renderBox({ value: 'a', onClear: () => { cleared = true; } });
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: '' } });
    expect(cleared).toBe(true);
  });

  test('Escape closes the list and does not bubble to the window', () => {
    let bubbled = false;
    const onKey = (e) => { if (e.key === 'Escape') bubbled = true; };
    window.addEventListener('keydown', onKey);
    try {
      const input = renderBox();
      fireEvent.focus(input);
      expect(list()).toBeTruthy();
      fireEvent.keyDown(input, { key: 'Escape' });
      expect(list()).toBeNull();
      expect(bubbled).toBe(false);
    } finally {
      window.removeEventListener('keydown', onKey);
    }
  });

  test('disabled renders no dropdown', () => {
    const input = renderBox({ value: 'a', disabled: true });
    expect(input).toBeDisabled();
    fireEvent.focus(input);
    expect(list()).toBeNull();
  });
});
