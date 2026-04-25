// Render-side proof that the "queued" pill appears on user turns whose
// POST landed in the per-chat queue (the in-flight turn was still
// running).  Backend semantics live in tests/http_controller.rs and
// the sessions reducers; this test only pins the user-facing surface
// — the SPA marks the user turn with `turn.queued = true` (and
// `turn.queuedCount` when N>1 sends merged into one bubble), and
// Turn renders the badge to mirror the server's coalesce behaviour
// (one merged user message → one agent reply).

import React from 'react';
import { describe, it, expect, afterEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { render, cleanup } from '@testing-library/react';
import { Turn } from '../components/turns.jsx';

const __dirname = dirname(fileURLToPath(import.meta.url));
const turnsCss = () =>
  readFileSync(join(__dirname, '..', 'styles', 'turns.css'), 'utf8');

afterEach(() => cleanup());

const userTurn = (extras = {}) => ({
  role: 'user',
  ts: '12:00:00',
  blocks: [{ type: 'text', text: 'while you are working' }],
  ...extras,
});

describe('queued badge — user-facing surface for queued POSTs', () => {
  it('plain user turn does not render the queued pill', () => {
    const { container } = render(
      <Turn turn={userTurn()} tools={{}} onOpenTool={() => {}} activeTool={null}/>
    );
    expect(container.querySelector('.queued-badge')).toBeNull();
  });

  it('a single queued send renders the pill without a count', () => {
    const { container } = render(
      <Turn turn={userTurn({ queued: true, queuedCount: 1 })} tools={{}}
            onOpenTool={() => {}} activeTool={null}/>
    );
    const badge = container.querySelector('.queued-badge');
    expect(badge, 'queued user turn must render .queued-badge').not.toBeNull();
    expect(badge.textContent).toBe('queued');
    expect(badge.getAttribute('title')).toMatch(/queued/i);
  });

  it('multi-send merge renders the count (queued ×N)', () => {
    const { container } = render(
      <Turn turn={userTurn({ queued: true, queuedCount: 3 })} tools={{}}
            onOpenTool={() => {}} activeTool={null}/>
    );
    const badge = container.querySelector('.queued-badge');
    expect(badge.textContent).toBe('queued ×3');
    // Tooltip explains the coalesce: server answers all in one reply.
    expect(badge.getAttribute('title')).toMatch(/3 messages.*one reply/i);
  });

  it('turn.queued without a queuedCount falls back to the singular pill', () => {
    const { container } = render(
      <Turn turn={userTurn({ queued: true })} tools={{}}
            onOpenTool={() => {}} activeTool={null}/>
    );
    expect(container.querySelector('.queued-badge').textContent).toBe('queued');
  });

  it('turns.css defines a .queued-badge selector so the pill is styled', () => {
    // jsdom has no CSS engine — assert the selector exists in source so
    // an accidental rename of the className doesn't ship an unstyled pill.
    const block = turnsCss().match(/\.turn \.queued-badge \{[^}]*\}/);
    expect(block, '.turn .queued-badge selector must exist in turns.css').toBeTruthy();
    expect(block[0]).toMatch(/border-radius:\s*999px/);
  });
});
