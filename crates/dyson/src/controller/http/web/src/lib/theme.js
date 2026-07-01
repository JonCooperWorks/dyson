/* Theme controller — three modes: 'system' | 'light' | 'dark'.
 *
 * The palette lives in dyson-common-ui/tokens.css, keyed off a `data-theme`
 * attribute on <html>.  "system" means *no* attribute (the CSS then follows
 * `prefers-color-scheme`); light/dark set it explicitly.  We mirror the choice
 * into the `theme-color` meta so mobile browser chrome matches.  An inline
 * script in index.html applies the saved choice before first paint to avoid a
 * flash — this module is the runtime source of truth after boot.
 *
 * The choice is stored in a cookie SHARED across the swarm apex and every
 * dyson subdomain (`<id>.<apex>`), so toggling the theme anywhere follows the
 * user to the swarm and all sibling dysons.  localStorage is a same-origin
 * fallback for when the cookie can't be domain-scoped (dev / bare hosts). */

const KEY = 'dyson-theme';
const COOKIE = 'dyson-theme';
export const MODES = ['system', 'light', 'dark'];

// Surface colour (--bg-1) per resolved theme, mirrored into <meta theme-color>.
const SURFACE = { dark: '#161922', light: '#ffffff' };

// Scope the shared cookie to the swarm host.  A dyson is served at
// `<instance-id>.<swarm-host>`, so strip the instance label to reach the
// swarm SPA + sibling dysons.  Returns null on a bare/IP host (dev), where the
// cookie stays host-only and localStorage carries the choice.
function cookieDomain() {
  const parent = location.hostname.replace(/^[^.]+\./, '').replace(/\.$/, '').toLowerCase();
  if (!parent.includes('.') || /^[0-9.]+$/.test(parent)) return null;
  return parent;
}

function readCookie() {
  const m = document.cookie.match(/(?:^|;\s*)dyson-theme=([^;]*)/);
  const v = m && decodeURIComponent(m[1]);
  return MODES.includes(v) ? v : null;
}

function writeCookie(mode) {
  const dom = cookieDomain();
  document.cookie = `${COOKIE}=${mode}; Path=/; Max-Age=31536000; SameSite=Lax`
    + (dom ? `; Domain=${dom}` : '')
    + (location.protocol === 'https:' ? '; Secure' : '');
}

export function getMode() {
  const shared = readCookie();
  if (shared) return shared;
  try {
    const v = localStorage.getItem(KEY);
    return MODES.includes(v) ? v : 'system';
  } catch {
    return 'system';
  }
}

// The concrete theme a mode resolves to right now ('light' | 'dark').
export function resolvedTheme(mode = getMode()) {
  if (mode === 'system') {
    return window.matchMedia?.('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  }
  return mode;
}

export function applyMode(mode) {
  const root = document.documentElement;
  if (mode === 'system') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', mode);

  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', SURFACE[resolvedTheme(mode)]);
}

export function setMode(mode) {
  const next = MODES.includes(mode) ? mode : 'system';
  try { localStorage.setItem(KEY, next); } catch { /* private mode — apply anyway */ }
  writeCookie(next);
  applyMode(next);
  return next;
}

/* Binary swap: flip to the opposite of whatever is showing now (resolving
   "system" against the OS first).  Always writes an explicit light/dark. */
export function toggleTheme() {
  return setMode(resolvedTheme() === 'dark' ? 'light' : 'dark');
}

/* Call once at boot: re-apply the saved mode (the inline script already set
   the attribute; this reconciles the meta tag) and keep "system" live by
   re-resolving the meta colour when the OS preference flips. */
export function initTheme() {
  applyMode(getMode());
  window.matchMedia?.('(prefers-color-scheme: light)').addEventListener?.('change', () => {
    if (getMode() === 'system') applyMode('system');
  });
}
