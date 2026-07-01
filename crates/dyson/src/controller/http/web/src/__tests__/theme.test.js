import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';

import { MODES, getMode, resolvedTheme, applyMode, setMode, cycleMode } from '../lib/theme.js';

// jsdom has no matchMedia; stub it so "system" resolution has something to read.
function stubMatchMedia(prefersLight) {
  window.matchMedia = vi.fn().mockImplementation((query) => ({
    matches: query.includes('light') ? prefersLight : !prefersLight,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute('data-theme');
  stubMatchMedia(false); // OS = dark
});

afterEach(() => {
  document.documentElement.removeAttribute('data-theme');
});

describe('theme controller', () => {
  test('defaults to system with no attribute set', () => {
    expect(getMode()).toBe('system');
    applyMode('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  test('explicit modes write the data-theme attribute', () => {
    setMode('light');
    expect(document.documentElement.getAttribute('data-theme')).toBe('light');
    setMode('dark');
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark');
  });

  test('system removes the attribute so the OS media query wins', () => {
    setMode('dark');
    setMode('system');
    expect(document.documentElement.hasAttribute('data-theme')).toBe(false);
  });

  test('choice persists to localStorage', () => {
    setMode('light');
    expect(getMode()).toBe('light');
  });

  test('cycles system -> light -> dark -> system', () => {
    expect(getMode()).toBe('system');
    expect(cycleMode()).toBe('light');
    expect(cycleMode()).toBe('dark');
    expect(cycleMode()).toBe('system');
  });

  test('resolvedTheme follows the OS in system mode', () => {
    stubMatchMedia(true); // OS = light
    expect(resolvedTheme('system')).toBe('light');
    stubMatchMedia(false); // OS = dark
    expect(resolvedTheme('system')).toBe('dark');
  });

  test('invalid stored value falls back to system', () => {
    localStorage.setItem('dyson-theme', 'chartreuse');
    expect(getMode()).toBe('system');
  });

  test('MODES lists the three supported modes', () => {
    expect(MODES).toEqual(['system', 'light', 'dark']);
  });
});
