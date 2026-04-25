// Render-side proof that the "queued" pill appears on user turns whose
// POST landed in the per-chat queue (the in-flight turn was still
// running).  Backend semantics live in tests/http_controller.rs and
// the sessions reducers; this test only pins the user-facing surface
// — the server returns `{queued: true, position: N}`, the SPA marks
// the user turn with `turn.queued = true, turn.queuedPosition = N`,
// and Turn renders the badge.

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

  it('turn.queued renders the pill with the position number', () => {
    const { container } = render(
      <Turn
        turn={userTurn({ queued: true, queuedPosition: 3 })}
        tools={{}} onOpenTool={() => {}} activeTool={null}/>
    );
    const badge = container.querySelector('.queued-badge');
    expect(badge, 'queued user turn must render .queued-badge').not.toBeNull();
    expect(badge.textContent).toBe('queued #3');
    // Tooltip helps a hovering user understand what queued means.
    expect(badge.getAttribute('title')).toMatch(/queued/i);
  });

  it('turn.queued without a position renders the pill without the number', () => {
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
