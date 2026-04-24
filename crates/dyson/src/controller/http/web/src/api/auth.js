/* Dyson — auth bootstrap for the SPA.
 *
 * Backend tells us via GET /api/auth/config which mode is active.
 * For OIDC we run a textbook Authorization Code + PKCE flow against
 * whatever IdP the operator pointed dyson at — Auth0, Okta, Entra,
 * Authentik, dex, take your pick.  Dyson never sees plaintext
 * credentials; it just verifies the JWT the IdP signs back to us.
 *
 * Storage:
 *   sessionStorage holds (a) the in-flight PKCE state during the
 *   redirect round-trip and (b) the access/refresh tokens once we
 *   have them.  sessionStorage is a deliberate choice: it survives
 *   the redirect (localStorage would too) but doesn't outlive the
 *   tab (a "logout" is just closing the window).  XSS would still
 *   hand an attacker the token; the deployment model here — single
 *   trusted operator behind loopback or Tailscale — accepts that
 *   risk.  Production multi-tenant deployments should run a BFF
 *   instead, but that's a different doc.
 *
 * Refresh:
 *   We schedule a silent refresh ~60 s before `expires_in` runs out.
 *   If refresh fails (token revoked, IdP down) we fall back to a
 *   full redirect — the user re-authenticates and continues.
 */

const STORAGE_KEY = 'dyson:auth';
const PENDING_KEY = 'dyson:auth:pending';
const REFRESH_LEEWAY_S = 60;

// ──────────────────────────────────────────────────────────────────
// Storage primitives — sessionStorage is per-tab; the tokens never
// leak to another tab the operator opens, and a tab close ends the
// session.
// ──────────────────────────────────────────────────────────────────

function readTokens() {
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch { return null; }
}

function writeTokens(tokens) {
  if (!tokens) sessionStorage.removeItem(STORAGE_KEY);
  else sessionStorage.setItem(STORAGE_KEY, JSON.stringify(tokens));
}

function readPending() {
  try {
    const raw = sessionStorage.getItem(PENDING_KEY);
    return raw ? JSON.parse(raw) : null;
  } catch { return null; }
}

function writePending(p) {
  if (!p) sessionStorage.removeItem(PENDING_KEY);
  else sessionStorage.setItem(PENDING_KEY, JSON.stringify(p));
}

// ──────────────────────────────────────────────────────────────────
// PKCE — RFC 7636.  S256 challenge from a cryptographically-random
// 32-byte verifier.  Both values are base64url-encoded with no
// padding so they survive a query string round-trip unmolested.
// ──────────────────────────────────────────────────────────────────

function base64url(bytes) {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function randomString(byteLen = 32) {
  const a = new Uint8Array(byteLen);
  crypto.getRandomValues(a);
  return base64url(a);
}

async function pkceChallenge(verifier) {
  const data = new TextEncoder().encode(verifier);
  const digest = await crypto.subtle.digest('SHA-256', data);
  return base64url(new Uint8Array(digest));
}

// ──────────────────────────────────────────────────────────────────
// Discovery — /api/auth/config is the unauthenticated endpoint the
// backend exposes specifically for this bootstrap step.
// ──────────────────────────────────────────────────────────────────

export async function loadAuthConfig() {
  const r = await fetch('/api/auth/config', { headers: { Accept: 'application/json' } });
  if (!r.ok) throw new Error(`/api/auth/config: ${r.status}`);
  return r.json();
}

// ──────────────────────────────────────────────────────────────────
// Auth code + PKCE redirect.  We park the verifier + state in
// sessionStorage so the IdP's redirect-back can prove (a) it's a
// reply to our request (state) and (b) we minted the matching
// challenge (verifier).
// ──────────────────────────────────────────────────────────────────

function redirectUri() {
  // Fixed callback path so the IdP can be configured once.  The SPA
  // is a single-route app at the URL root — `?code=…&state=…` lands
  // on that same route; we detect it and short-circuit out of the
  // normal mount path.
  return `${window.location.origin}/`;
}

async function startAuthorizationFlow(cfg) {
  const verifier = randomString(32);
  const challenge = await pkceChallenge(verifier);
  const state = randomString(16);
  // Capture the user's original location so we can put them back
  // after the redirect — otherwise every login lands on `#/`.
  writePending({
    verifier,
    state,
    returnTo: window.location.hash || '#/',
    issuedAt: Date.now(),
  });

  const scopes = ['openid', ...(cfg.required_scopes || [])];
  // Some IdPs require `offline_access` to issue a refresh token.
  // Adding it unconditionally is harmless on providers that ignore
  // unknown scopes; providers that reject it can be configured
  // out via dyson.json's required_scopes (we still add openid).
  if (!scopes.includes('offline_access')) scopes.push('offline_access');

  const url = new URL(cfg.authorization_endpoint);
  url.searchParams.set('response_type', 'code');
  url.searchParams.set('client_id', cfg.client_id);
  url.searchParams.set('redirect_uri', redirectUri());
  url.searchParams.set('scope', scopes.join(' '));
  url.searchParams.set('code_challenge', challenge);
  url.searchParams.set('code_challenge_method', 'S256');
  url.searchParams.set('state', state);
  window.location.assign(url.toString());
}

// ──────────────────────────────────────────────────────────────────
// Callback — on return from the IdP, exchange code for tokens.
// Returns null when the URL doesn't carry a callback (the common
// case: cold load with valid tokens already in storage).
// ──────────────────────────────────────────────────────────────────

async function handleCallback(cfg) {
  const params = new URLSearchParams(window.location.search);
  const code = params.get('code');
  const state = params.get('state');
  if (!code) return null;

  const pending = readPending();
  writePending(null);
  if (!pending || pending.state !== state) {
    // Stale or forged callback — wipe everything and restart.
    writeTokens(null);
    throw new Error('auth callback: state mismatch');
  }
  if (!cfg.token_endpoint) {
    throw new Error('auth callback: provider has no token_endpoint');
  }

  const body = new URLSearchParams({
    grant_type: 'authorization_code',
    code,
    redirect_uri: redirectUri(),
    client_id: cfg.client_id,
    code_verifier: pending.verifier,
  });
  const r = await fetch(cfg.token_endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body.toString(),
  });
  if (!r.ok) {
    const detail = await r.text().catch(() => '');
    throw new Error(`token exchange failed: ${r.status}${detail ? ` — ${detail}` : ''}`);
  }
  const tokens = await r.json();
  writeTokens(toStorageShape(tokens));

  // Strip ?code=&state= and restore the pre-redirect location so
  // refreshes don't re-trigger the exchange (and so deep-links into
  // a chat survive).
  const url = new URL(window.location.href);
  url.search = '';
  url.hash = pending.returnTo || '';
  window.history.replaceState(null, '', url.toString());
  return tokens;
}

function toStorageShape(t) {
  // `expires_in` is seconds-from-now; convert to absolute epoch ms
  // so a slow-running tab doesn't keep using a token past its real
  // expiry just because Date.now() advanced while sleeping.
  const expSec = typeof t.expires_in === 'number' ? t.expires_in : 3600;
  return {
    access_token: t.access_token,
    refresh_token: t.refresh_token || null,
    expires_at: Date.now() + expSec * 1000,
    token_type: t.token_type || 'Bearer',
    scope: t.scope || null,
  };
}

// ──────────────────────────────────────────────────────────────────
// Refresh — silent, before the access token expires.  Falls through
// to a full re-redirect if the refresh fails (revoked, IdP down).
// ──────────────────────────────────────────────────────────────────

async function refreshTokens(cfg) {
  const tokens = readTokens();
  if (!tokens?.refresh_token || !cfg.token_endpoint) return null;
  const body = new URLSearchParams({
    grant_type: 'refresh_token',
    refresh_token: tokens.refresh_token,
    client_id: cfg.client_id,
  });
  const r = await fetch(cfg.token_endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
    body: body.toString(),
  });
  if (!r.ok) return null;
  const next = await r.json();
  // Some IdPs rotate the refresh token, others don't — preserve the
  // existing one when the response omits it.
  const merged = toStorageShape(next);
  if (!merged.refresh_token) merged.refresh_token = tokens.refresh_token;
  writeTokens(merged);
  return merged;
}

function scheduleRefresh(cfg, onRedirect) {
  const tokens = readTokens();
  if (!tokens?.access_token) return;
  const ms = tokens.expires_at - Date.now() - REFRESH_LEEWAY_S * 1000;
  if (ms <= 0) {
    // Already too close to expiry — try once now, redirect if it
    // doesn't pan out.
    refreshTokens(cfg).then(t => {
      if (t) scheduleRefresh(cfg, onRedirect);
      else onRedirect();
    });
    return;
  }
  setTimeout(() => {
    refreshTokens(cfg).then(t => {
      if (t) scheduleRefresh(cfg, onRedirect);
      else onRedirect();
    });
  }, ms);
}

// ──────────────────────────────────────────────────────────────────
// AuthSession — what main.jsx waits on before mounting React.
// Returns { mode, getToken, logout } once the SPA has whatever
// credential the controller demands.
// ──────────────────────────────────────────────────────────────────

export async function bootstrapAuth() {
  const cfg = await loadAuthConfig();

  // No auth required — the controller is in dangerous_no_auth or
  // bearer mode (bearer means a header is required, but the SPA
  // doesn't have a way to source the plaintext for the user;
  // operators using bearer mode are expected to use a CLI / curl
  // and an out-of-band token, not this UI).
  if (cfg.mode === 'none' || cfg.mode === 'bearer') {
    return {
      mode: cfg.mode,
      getToken: () => null,
      logout: () => {},
    };
  }

  if (cfg.mode !== 'oidc') {
    throw new Error(`unknown auth mode: ${cfg.mode}`);
  }

  // Fast path — already have a usable token in this tab.
  let tokens = readTokens();

  // Slow paths — finish a redirect, refresh, or start fresh.
  if (!tokens) {
    tokens = await handleCallback(cfg);
  }
  if (tokens && tokens.expires_at - Date.now() < REFRESH_LEEWAY_S * 1000) {
    tokens = await refreshTokens(cfg);
  }
  if (!tokens?.access_token) {
    await startAuthorizationFlow(cfg);
    // startAuthorizationFlow navigates away — anything after this
    // line runs only in tests or if the redirect was blocked.
    return new Promise(() => {});
  }

  const onRedirect = () => startAuthorizationFlow(cfg).catch(() => {});
  scheduleRefresh(cfg, onRedirect);

  return {
    mode: 'oidc',
    getToken: () => readTokens()?.access_token || null,
    logout: () => {
      writeTokens(null);
      writePending(null);
      onRedirect();
    },
  };
}
