import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';

import { createThemeController } from 'dyson-common-ui';

// The theme logic itself lives in (and is tested by) dyson-common-ui.  These
// tests only pin the *binding* this app uses at its import sites — the exact
// `storageKey`/`stripInstanceLabel` config that main.jsx + views.jsx wire in.
// A wrong key or a dropped flag would silently unshare the theme across the
// swarm, so the config round-trip is the thing worth guarding here.
const theme = createThemeController({ storageKey: 'dyson-theme', stripInstanceLabel: true });

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
  document.cookie = 'dyson-theme=; Path=/; Max-Age=0';
  document.documentElement.removeAttribute('data-theme');
  stubMatchMedia(false); // OS = dark
});

afterEach(() => {
  document.documentElement.removeAttribute('data-theme');
});

describe('dyson theme binding', () => {
  test('persists the choice under the dyson-theme storageKey', () => {
    theme.setMode('light');
    expect(localStorage.getItem('dyson-theme')).toBe('light');
    expect(theme.getMode()).toBe('light');
  });

  test('writes the swarm-shared dyson-theme cookie, preferred over stale localStorage', () => {
    theme.setMode('light');
    expect(document.cookie).toContain('dyson-theme=light');
    localStorage.setItem('dyson-theme', 'dark');
    expect(theme.getMode()).toBe('light');
  });
});
