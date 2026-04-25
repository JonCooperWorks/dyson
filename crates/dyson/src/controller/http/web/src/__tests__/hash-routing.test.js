/* Hash routing — URL ↔ state contract.
 *
 * These tests pin the shape of the hash so deep-links round-trip:
 *   #/                            conv view, no chat
 *   #/c/<id>                       conv view, specific chat
 *   #/c/<id>/t/<toolRef>           conv view, chat + tool panel open
 *   #/mind                         mind view
 *   #/mind/<path>                  mind view with a specific file open
 *   #/artefacts                    artefacts list
 *   #/artefacts/<id>               artefacts reader open on that id
 *
 * Tool deep-links and mind-path deep-links are the two new shapes —
 * clicking a tool in chat or selecting a workspace file used to be
 * untracked state, so the back button skipped past them and a
 * shared URL didn't preserve them.  The tests below assert the
 * round-trip in both directions (parse and build).
 */

import { describe, it, expect } from 'vitest';
import { parseHash, buildHash } from '../components/app.jsx';

describe('parseHash', () => {
  it('empty hash → conv view, no chat', () => {
    expect(parseHash('')).toEqual({
      view: 'conv', conv: null, artefactId: null, toolRef: null, mindPath: null,
    });
  });

  it('#/c/<id> → conv view + chat id', () => {
    expect(parseHash('#/c/c-0001')).toEqual({
      view: 'conv', conv: 'c-0001', artefactId: null, toolRef: null, mindPath: null,
    });
  });

  it('#/c/<id>/t/<ref> → conv view + chat + tool open', () => {
    // The toolRef is a stable per-turn id (the agent's tool_use_id).
    // Clicking a tool chip should push this hash; the back button
    // pops off the tool panel without leaving the chat.
    expect(parseHash('#/c/c-0001/t/tool_42')).toEqual({
      view: 'conv', conv: 'c-0001', artefactId: null, toolRef: 'tool_42', mindPath: null,
    });
  });

  it('#/mind → mind view, no file', () => {
    expect(parseHash('#/mind')).toEqual({
      view: 'mind', conv: null, artefactId: null, toolRef: null, mindPath: null,
    });
  });

  it('#/mind/<path> → mind view + selected file (multi-segment, encoded)', () => {
    // Workspace files can live in subdirs (memory/SOUL.md), so the
    // path uses everything after `mind/` and is URL-encoded.
    expect(parseHash('#/mind/memory%2FSOUL.md')).toEqual({
      view: 'mind', conv: null, artefactId: null, toolRef: null, mindPath: 'memory/SOUL.md',
    });
    expect(parseHash('#/mind/IDENTITY.md')).toEqual({
      view: 'mind', conv: null, artefactId: null, toolRef: null, mindPath: 'IDENTITY.md',
    });
  });

  it('#/artefacts and #/artefacts/<id> stay backward-compatible', () => {
    expect(parseHash('#/artefacts')).toEqual({
      view: 'artefacts', conv: null, artefactId: null, toolRef: null, mindPath: null,
    });
    expect(parseHash('#/artefacts/a1')).toEqual({
      view: 'artefacts', conv: null, artefactId: 'a1', toolRef: null, mindPath: null,
    });
  });
});

describe('buildHash', () => {
  it('round-trips the conv shape', () => {
    expect(buildHash({ view: 'conv', conv: null })).toBe('#/');
    expect(buildHash({ view: 'conv', conv: 'c-0001' })).toBe('#/c/c-0001');
  });

  it('round-trips conv + tool', () => {
    expect(buildHash({ view: 'conv', conv: 'c-0001', toolRef: 'tool_42' }))
      .toBe('#/c/c-0001/t/tool_42');
  });

  it('round-trips mind view, with and without a path', () => {
    expect(buildHash({ view: 'mind' })).toBe('#/mind');
    expect(buildHash({ view: 'mind', mindPath: 'memory/SOUL.md' }))
      .toBe('#/mind/memory%2FSOUL.md');
  });

  it('round-trips artefacts view, with and without an id', () => {
    expect(buildHash({ view: 'artefacts' })).toBe('#/artefacts');
    expect(buildHash({ view: 'artefacts', artefactId: 'a1' }))
      .toBe('#/artefacts/a1');
  });
});

describe('parse → build round trip', () => {
  it('every documented shape parses and rebuilds to the same hash', () => {
    const shapes = [
      '#/',
      '#/c/c-0001',
      '#/c/c-0001/t/tool_42',
      '#/mind',
      '#/mind/memory%2FSOUL.md',
      '#/artefacts',
      '#/artefacts/a1',
    ];
    for (const h of shapes) {
      const parsed = parseHash(h);
      expect(buildHash(parsed), `${h} must round-trip`).toBe(h);
    }
  });
});
