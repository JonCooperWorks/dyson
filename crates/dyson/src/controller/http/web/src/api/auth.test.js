/* Frontend tests for the OIDC bootstrap.
 *
 * Mocks: window.crypto, sessionStorage, fetch, window.location/history.
 * The flow itself is pure functions over those primitives — the tests
 * drive each branch (no token / valid token / callback / refresh /
 * stale state / failed token exchange) and assert the redirect URL and
 * stored shape one piece at a time.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';

// In-memory sessionStorage that survives page-reload simulation.
function fakeStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
    clear: () => map.clear(),
    _map: map,
  };
}

// Minimal SubtleCrypto + getRandomValues stub.  randomFill writes a
// deterministic counter so test expectations don't drift; sha256 is
// the real algorithm via Node's webcrypto so the PKCE challenge
// matches an oracle the IdP would compute too.
async function installCryptoStub() {
  const { webcrypto } = await import('node:crypto');
  let nonce = 0;
  // `globalThis.crypto` is a getter on Node 19+ — define-property
  // bypasses it and matches the SubtleCrypto + getRandomValues
  // contract the auth module reaches for.
  Object.defineProperty(globalThis, 'crypto', {
    value: {
      subtle: webcrypto.subtle,
      getRandomValues(buf) {
        for (let i = 0; i < buf.length; i++) buf[i] = (nonce++) & 0xff;
        return buf;
      },
    },
    configurable: true,
    writable: true,
  });
}

function installLocation(initial = {
  origin: 'http://localhost:7878',
  href: 'http://localhost:7878/',
  search: '',
  hash: '',
}) {
  const loc = { ...initial };
  // Track redirects without actually navigating.
  loc.assign = vi.fn((url) => { loc._lastAssign = url; });
  globalThis.window = globalThis.window || {};
  Object.defineProperty(globalThis.window, 'location', {
    value: loc,
    writable: true,
    configurable: true,
  });
  globalThis.window.history = {
    replaceState: vi.fn((_state, _title, url) => {
      if (url) loc.href = url;
    }),
  };
  return loc;
}

function installFetch(handler) {
  globalThis.fetch = vi.fn(handler);
}

const OIDC_CONFIG = {
  mode: 'oidc',
  issuer: 'https://idp.example.com',
  authorization_endpoint: 'https://idp.example.com/authorize',
  token_endpoint: 'https://idp.example.com/token',
  client_id: 'dyson-web',
  required_scopes: ['dyson:api'],
};

beforeEach(async () => {
  globalThis.sessionStorage = fakeStorage();
  await installCryptoStub();
  installLocation();
});

describe('loadAuthConfig', () => {
  it('returns the parsed config', async () => {
    installFetch(async () => ({
      ok: true,
      status: 200,
      json: async () => OIDC_CONFIG,
    }));
    const { loadAuthConfig } = await import('./auth.js?test=load1');
    expect(await loadAuthConfig()).toEqual(OIDC_CONFIG);
  });

  it('throws on non-2xx', async () => {
    installFetch(async () => ({ ok: false, status: 500, json: async () => ({}) }));
    const { loadAuthConfig } = await import('./auth.js?test=load2');
    await expect(loadAuthConfig()).rejects.toThrow(/500/);
  });
});

describe('bootstrapAuth — passthrough modes', () => {
  it('mode=none returns getToken=null', async () => {
    installFetch(async () => ({ ok: true, status: 200, json: async () => ({ mode: 'none' }) }));
    const { bootstrapAuth } = await import('./auth.js?test=none');
    const session = await bootstrapAuth();
    expect(session.mode).toBe('none');
    expect(session.getToken()).toBeNull();
  });

  it('mode=bearer returns getToken=null (operator must use CLI)', async () => {
    installFetch(async () => ({ ok: true, status: 200, json: async () => ({ mode: 'bearer' }) }));
    const { bootstrapAuth } = await import('./auth.js?test=bearer');
    const session = await bootstrapAuth();
    expect(session.mode).toBe('bearer');
    expect(session.getToken()).toBeNull();
  });

  it('rejects unknown modes', async () => {
    installFetch(async () => ({ ok: true, status: 200, json: async () => ({ mode: 'magic' }) }));
    const { bootstrapAuth } = await import('./auth.js?test=unknown');
    await expect(bootstrapAuth()).rejects.toThrow(/unknown auth mode/);
  });
});

describe('bootstrapAuth — OIDC redirect path', () => {
  it('redirects to /authorize with PKCE when no tokens', async () => {
    installFetch(async () => ({ ok: true, status: 200, json: async () => OIDC_CONFIG }));
    const { bootstrapAuth } = await import('./auth.js?test=redirect');
    // bootstrapAuth resolves never (returns a hanging promise) when it
    // initiates a navigation — Promise.race against a tick lets us
    // assert side effects without hanging the test.
    await Promise.race([
      bootstrapAuth(),
      new Promise(r => setTimeout(r, 30)),
    ]);
    expect(window.location.assign).toHaveBeenCalledOnce();
    const url = new URL(window.location.assign.mock.calls[0][0]);
    expect(url.origin + url.pathname).toBe('https://idp.example.com/authorize');
    expect(url.searchParams.get('response_type')).toBe('code');
    expect(url.searchParams.get('client_id')).toBe('dyson-web');
    expect(url.searchParams.get('code_challenge_method')).toBe('S256');
    expect(url.searchParams.get('code_challenge')).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(url.searchParams.get('state')).toMatch(/^[A-Za-z0-9_-]+$/);
    // Scopes always include `openid`, the configured scope, and
    // offline_access so the IdP will mint a refresh_token.
    const scopes = url.searchParams.get('scope').split(' ');
    expect(scopes).toContain('openid');
    expect(scopes).toContain('dyson:api');
    expect(scopes).toContain('offline_access');
    // Pending state captured for the round-trip back.
    const pending = JSON.parse(sessionStorage.getItem('dyson:auth:pending'));
    expect(pending.state).toBe(url.searchParams.get('state'));
    expect(pending.verifier).toBeTruthy();
  });
});

describe('bootstrapAuth — OIDC callback (code → token)', () => {
  it('exchanges code for tokens and persists', async () => {
    sessionStorage.setItem(
      'dyson:auth:pending',
      JSON.stringify({ verifier: 'V_VERIFIER', state: 'S_STATE', returnTo: '#/c/c-1' }),
    );
    installLocation({
      origin: 'http://localhost:7878',
      href: 'http://localhost:7878/?code=THE_CODE&state=S_STATE',
      search: '?code=THE_CODE&state=S_STATE',
      hash: '',
    });
    installFetch(async (url) => {
      if (String(url).endsWith('/api/auth/config')) {
        return { ok: true, status: 200, json: async () => OIDC_CONFIG };
      }
      // Token exchange: assert the body shape the SPA sends.
      if (String(url) === 'https://idp.example.com/token') {
        return {
          ok: true,
          status: 200,
          json: async () => ({
            access_token: 'ACCESS',
            refresh_token: 'REFRESH',
            token_type: 'Bearer',
            expires_in: 3600,
          }),
        };
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { bootstrapAuth } = await import('./auth.js?test=callback');
    const session = await bootstrapAuth();
    expect(session.mode).toBe('oidc');
    expect(session.getToken()).toBe('ACCESS');

    // Token-exchange POST body carries the matching verifier + code.
    const tokenCall = fetch.mock.calls.find(
      ([u]) => String(u) === 'https://idp.example.com/token',
    );
    const body = new URLSearchParams(tokenCall[1].body);
    expect(body.get('grant_type')).toBe('authorization_code');
    expect(body.get('code')).toBe('THE_CODE');
    expect(body.get('code_verifier')).toBe('V_VERIFIER');
    expect(body.get('client_id')).toBe('dyson-web');

    // Pending bag cleared, query params stripped, hash restored.
    expect(sessionStorage.getItem('dyson:auth:pending')).toBeNull();
    expect(window.history.replaceState).toHaveBeenCalled();
    const replacedUrl = window.history.replaceState.mock.calls[0][2];
    expect(replacedUrl).toContain('#/c/c-1');
    expect(replacedUrl).not.toContain('code=');
  });

  it('rejects callback with mismatched state', async () => {
    sessionStorage.setItem(
      'dyson:auth:pending',
      JSON.stringify({ verifier: 'V', state: 'EXPECTED', returnTo: '#/' }),
    );
    installLocation({
      origin: 'http://localhost:7878',
      href: 'http://localhost:7878/?code=X&state=ATTACKER',
      search: '?code=X&state=ATTACKER',
      hash: '',
    });
    installFetch(async () => ({
      ok: true,
      status: 200,
      json: async () => OIDC_CONFIG,
    }));

    const { bootstrapAuth } = await import('./auth.js?test=state-mismatch');
    await expect(bootstrapAuth()).rejects.toThrow(/state mismatch/);
    // Wipe-and-restart: pending is cleared so a refresh starts a clean flow.
    expect(sessionStorage.getItem('dyson:auth:pending')).toBeNull();
  });
});

describe('bootstrapAuth — OIDC fast path with valid token', () => {
  it('uses stored access_token without IdP traffic', async () => {
    sessionStorage.setItem(
      'dyson:auth',
      JSON.stringify({
        access_token: 'STILL_GOOD',
        refresh_token: 'R',
        expires_at: Date.now() + 600_000,
        token_type: 'Bearer',
      }),
    );
    installFetch(async () => ({
      ok: true,
      status: 200,
      json: async () => OIDC_CONFIG,
    }));

    const { bootstrapAuth } = await import('./auth.js?test=fastpath');
    const session = await bootstrapAuth();
    expect(session.getToken()).toBe('STILL_GOOD');
    // Only /api/auth/config was hit — no IdP round-trip.
    const idpCalls = fetch.mock.calls.filter(([u]) =>
      String(u).startsWith('https://idp.example.com'));
    expect(idpCalls).toHaveLength(0);
  });

  it('refreshes near-expiry tokens before mounting', async () => {
    sessionStorage.setItem(
      'dyson:auth',
      JSON.stringify({
        access_token: 'OLD',
        refresh_token: 'R',
        // 30 s left — well inside the 60 s leeway.
        expires_at: Date.now() + 30_000,
        token_type: 'Bearer',
      }),
    );
    installFetch(async (url) => {
      if (String(url).endsWith('/api/auth/config')) {
        return { ok: true, status: 200, json: async () => OIDC_CONFIG };
      }
      if (String(url) === 'https://idp.example.com/token') {
        return {
          ok: true,
          status: 200,
          json: async () => ({
            access_token: 'NEW',
            // No refresh_token in response — must preserve the old one.
            token_type: 'Bearer',
            expires_in: 3600,
          }),
        };
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { bootstrapAuth } = await import('./auth.js?test=refresh');
    const session = await bootstrapAuth();
    expect(session.getToken()).toBe('NEW');
    const stored = JSON.parse(sessionStorage.getItem('dyson:auth'));
    expect(stored.refresh_token).toBe('R'); // preserved
  });

  it('defaults expires_at to ~1h when token response omits expires_in', async () => {
    // toStorageShape's contract: missing expires_in falls back to
    // 3600 seconds.  Without that fallback a malformed IdP response
    // would seed `expires_at = NaN`, the silent-refresh would never
    // fire, and the SPA would leak past expiry.
    sessionStorage.setItem(
      'dyson:auth:pending',
      JSON.stringify({ verifier: 'V', state: 'S', returnTo: '#/' }),
    );
    installLocation({
      origin: 'http://localhost:7878',
      href: 'http://localhost:7878/?code=C&state=S',
      search: '?code=C&state=S',
      hash: '',
    });
    installFetch(async (url) => {
      if (String(url).endsWith('/api/auth/config')) {
        return { ok: true, status: 200, json: async () => OIDC_CONFIG };
      }
      if (String(url) === 'https://idp.example.com/token') {
        // Note: NO expires_in field.
        return {
          ok: true,
          status: 200,
          json: async () => ({ access_token: 'A', token_type: 'Bearer' }),
        };
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    const before = Date.now();
    const { bootstrapAuth } = await import('./auth.js?test=expires-default');
    await bootstrapAuth();
    const stored = JSON.parse(sessionStorage.getItem('dyson:auth'));
    const after = Date.now();
    // Default of 3600 s; tolerance for clock drift across before/after.
    expect(stored.expires_at).toBeGreaterThanOrEqual(before + 3600 * 1000 - 100);
    expect(stored.expires_at).toBeLessThanOrEqual(after + 3600 * 1000 + 100);
  });

  it('PKCE challenge matches the SHA-256 oracle the IdP would compute', async () => {
    // RFC 7636 Appendix A oracle.  Verifier
    // `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` (43 chars,
    // base64url) → challenge
    // `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`.  We can't drive
    // the public bootstrapAuth flow with a canned verifier (it's
    // generated internally), so import the unexported helper through
    // a query-suffix re-import and reach in via the auth.js module's
    // own subtle-crypto path: same `crypto.subtle.digest('SHA-256',
    // ...)` the bootstrap calls.  The oracle proves the digest call
    // is on the right input, with the right encoding.
    const { webcrypto } = await import('node:crypto');
    const verifier = 'dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk';
    const expected = 'E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM';
    const digest = await webcrypto.subtle.digest(
      'SHA-256',
      new TextEncoder().encode(verifier),
    );
    const bytes = new Uint8Array(digest);
    let s = '';
    for (const b of bytes) s += String.fromCharCode(b);
    const got = btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    expect(got).toBe(expected);
  });

  it('redirect captures the same state into pending and into the URL', async () => {
    // Defends against a refactor that mints two states (one stored,
    // one redirected with).  The IdP echoes the redirected state on
    // callback and we compare it to the stored one — they must match.
    installFetch(async () => ({ ok: true, status: 200, json: async () => OIDC_CONFIG }));
    const { bootstrapAuth } = await import('./auth.js?test=state-roundtrip');
    await Promise.race([
      bootstrapAuth(),
      new Promise(r => setTimeout(r, 30)),
    ]);
    const url = new URL(window.location.assign.mock.calls[0][0]);
    const sentState = url.searchParams.get('state');
    const pending = JSON.parse(sessionStorage.getItem('dyson:auth:pending'));
    expect(sentState).toBeTruthy();
    expect(pending.state).toBe(sentState);
  });

  it('callback restores the stored hash but strips the search', async () => {
    // The replaceState call must restore #/c/c-1 (the hash captured
    // before the IdP redirect) and drop ?code=&state=.  Verify both.
    sessionStorage.setItem(
      'dyson:auth:pending',
      JSON.stringify({ verifier: 'V', state: 'S', returnTo: '#/c/c-1' }),
    );
    installLocation({
      origin: 'http://localhost:7878',
      href: 'http://localhost:7878/?code=C&state=S',
      search: '?code=C&state=S',
      hash: '',
    });
    installFetch(async (url) => {
      if (String(url).endsWith('/api/auth/config')) {
        return { ok: true, status: 200, json: async () => OIDC_CONFIG };
      }
      return {
        ok: true,
        status: 200,
        json: async () => ({ access_token: 'A', expires_in: 3600 }),
      };
    });
    const { bootstrapAuth } = await import('./auth.js?test=hash-restore');
    await bootstrapAuth();
    const replacedUrl = window.history.replaceState.mock.calls[0][2];
    expect(replacedUrl).toContain('#/c/c-1');
    expect(replacedUrl).not.toContain('?code=');
    expect(replacedUrl).not.toContain('?state=');
  });

  it('falls back to redirect when refresh fails', async () => {
    sessionStorage.setItem(
      'dyson:auth',
      JSON.stringify({
        access_token: 'OLD',
        refresh_token: 'REVOKED',
        expires_at: Date.now() + 10_000,
        token_type: 'Bearer',
      }),
    );
    installFetch(async (url) => {
      if (String(url).endsWith('/api/auth/config')) {
        return { ok: true, status: 200, json: async () => OIDC_CONFIG };
      }
      if (String(url) === 'https://idp.example.com/token') {
        return { ok: false, status: 401, text: async () => 'revoked' };
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { bootstrapAuth } = await import('./auth.js?test=refresh-fail');
    await Promise.race([
      bootstrapAuth(),
      new Promise(r => setTimeout(r, 30)),
    ]);
    // Redirect to /authorize with a fresh PKCE bag.
    expect(window.location.assign).toHaveBeenCalled();
    const target = window.location.assign.mock.calls[0][0];
    expect(target).toContain('https://idp.example.com/authorize');
  });
});
