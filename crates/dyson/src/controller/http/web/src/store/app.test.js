import { describe, it, expect, beforeEach } from 'vitest';
import {
  app,
  setLive,
  setConversations,
  upsertConversation,
  removeConversation,
  markConversationHasArtefacts,
  setProviders,
  switchProviderModel,
  setMind,
  setActivity,
  setTool,
  updateTool,
  requestOpenArtefact,
  clearPendingArtefact,
  requestOpenRail,
  requestToggleArtefactsDrawer,
  __resetAppStoreForTests,
} from './app.js';

beforeEach(() => {
  __resetAppStoreForTests();
});

describe('app store — conversations', () => {
  it('setConversations replaces the list', () => {
    setConversations([{ id: 'a', title: 'Alpha' }]);
    expect(app.getSnapshot().conversations.map(c => c.id)).toEqual(['a']);
  });

  it('upsertConversation prepends new rows and merges existing ones', () => {
    setConversations([{ id: 'a', title: 'Alpha' }]);
    upsertConversation({ id: 'b', title: 'Beta' });
    expect(app.getSnapshot().conversations.map(c => c.id)).toEqual(['b', 'a']);
    upsertConversation({ id: 'a', title: 'Alpha renamed' });
    const rows = app.getSnapshot().conversations;
    expect(rows.find(c => c.id === 'a').title).toBe('Alpha renamed');
  });

  it('removeConversation drops by id', () => {
    setConversations([{ id: 'a' }, { id: 'b' }]);
    removeConversation('a');
    expect(app.getSnapshot().conversations.map(c => c.id)).toEqual(['b']);
  });

  it('markConversationHasArtefacts flips the flag without churning the list when already set', () => {
    setConversations([{ id: 'a' }]);
    const before = app.getSnapshot();
    markConversationHasArtefacts('a');
    expect(app.getSnapshot().conversations[0].hasArtefacts).toBe(true);
    const mid = app.getSnapshot();
    markConversationHasArtefacts('a');
    // Idempotent: same reference = dispatch no-op.
    expect(app.getSnapshot()).toBe(mid);
    expect(app.getSnapshot()).not.toBe(before);
  });
});

describe('app store — providers and model', () => {
  it('setProviders sets both the list and the active model', () => {
    setProviders([{ id: 'p1', active: true, activeModel: 'm1', models: ['m1', 'm2'] }], 'm1');
    expect(app.getSnapshot().activeModel).toBe('m1');
    expect(app.getSnapshot().providers[0].id).toBe('p1');
  });

  it('switchProviderModel updates the active flag and model', () => {
    setProviders([
      { id: 'p1', active: true, activeModel: 'm1' },
      { id: 'p2', active: false, activeModel: 'm2' },
    ], 'm1');
    switchProviderModel('p2', 'm2');
    const { activeModel, providers } = app.getSnapshot();
    expect(activeModel).toBe('m2');
    expect(providers.find(p => p.id === 'p1').active).toBe(false);
    expect(providers.find(p => p.id === 'p2').active).toBe(true);
    expect(providers.find(p => p.id === 'p2').activeModel).toBe('m2');
  });
});

describe('app store — tools', () => {
  it('setTool installs by ref', () => {
    setTool('a-live-1', { name: 'bash', status: 'running' });
    expect(app.getSnapshot().tools['a-live-1'].name).toBe('bash');
  });

  it('updateTool merges onto an existing entry', () => {
    setTool('t', { name: 'bash', status: 'running', body: { text: 'hi' } });
    updateTool('t', { status: 'done', exit: 'ok' });
    const t = app.getSnapshot().tools['t'];
    expect(t.status).toBe('done');
    expect(t.exit).toBe('ok');
    expect(t.body.text).toBe('hi');
  });

  it('updateTool is a no-op when the ref is unknown', () => {
    const before = app.getSnapshot();
    updateTool('missing', { status: 'done' });
    expect(app.getSnapshot()).toBe(before);
  });

  it('updateTool accepts a reducer for nested rewrites', () => {
    setTool('t', { name: 'bash', body: { text: 'a' } });
    updateTool('t', existing => ({ ...existing, body: { ...existing.body, text: existing.body.text + 'b' } }));
    expect(app.getSnapshot().tools['t'].body.text).toBe('ab');
  });
});

describe('app store — live / mind / activity / UI nonces', () => {
  it('setLive toggles the flag and is idempotent', () => {
    setLive(true);
    const s = app.getSnapshot();
    setLive(true);
    expect(app.getSnapshot()).toBe(s);
    setLive(false);
    expect(app.getSnapshot().live).toBe(false);
  });

  it('setMind merges onto the previous mind shape', () => {
    setMind({ backend: 'fs', files: [{ path: 'a.md' }] });
    expect(app.getSnapshot().mind.backend).toBe('fs');
    setMind({ files: [] });
    expect(app.getSnapshot().mind.backend).toBe('fs');
    expect(app.getSnapshot().mind.files).toEqual([]);
  });

  it('setActivity replaces lanes', () => {
    setActivity([{ name: 'x', status: 'running' }]);
    expect(app.getSnapshot().activity).toHaveLength(1);
  });

  it('requestOpenArtefact + clearPendingArtefact toggle the pending id', () => {
    requestOpenArtefact('a1');
    expect(app.getSnapshot().ui.pendingArtefactId).toBe('a1');
    clearPendingArtefact();
    expect(app.getSnapshot().ui.pendingArtefactId).toBeNull();
  });

  it('open-rail / toggle-artefacts nonces monotonically increase', () => {
    const a = app.getSnapshot().ui.openRailNonce;
    requestOpenRail();
    expect(app.getSnapshot().ui.openRailNonce).toBe(a + 1);
    const b = app.getSnapshot().ui.toggleArtefactsDrawerNonce;
    requestToggleArtefactsDrawer();
    expect(app.getSnapshot().ui.toggleArtefactsDrawerNonce).toBe(b + 1);
  });
});

describe('app store — snapshot immutability', () => {
  it('snapshot fields cannot be mutated in place', () => {
    setConversations([{ id: 'a' }]);
    const snap = app.getSnapshot();
    expect(() => { snap.conversations.push({ id: 'b' }); }).toThrow();
    expect(() => { snap.conversations[0].id = 'c'; }).toThrow();
  });
});
