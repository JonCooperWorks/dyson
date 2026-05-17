// UX tweaks:
//   1. Copy-from-anywhere: the per-turn copy button used to scroll out
//      of view inside long agent messages — `.turn .copy-turn` is now
//      `position: sticky` so the header (with its copy button) stays
//      pinned to the transcript top while you scroll a turn.
//   2. Inline tool blocks: tool calls render expandable in the
//      transcript (no right-rail tool stack).  Clicking the chip flips
//      `aria-expanded` and reveals the kind-specific body underneath.
//
// jsdom has no CSS engine and no layout, so the sticky header is
// asserted via a source-text grep on turns.css (matches the regression
// test style next door).  The expand wiring is exercised with a real
// React mount.

import React from 'react';
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { render, fireEvent, cleanup } from '@testing-library/react';
import { RunStatusStrip, ToolBlock } from '../components/turns.jsx';

const __dirname = dirname(fileURLToPath(import.meta.url));
const turnsCss = () => readFileSync(join(__dirname, '..', 'styles', 'turns.css'), 'utf8');

afterEach(() => { cleanup(); });

describe('UX: copy-from-anywhere — sticky copy button', () => {
  it('.turn .copy-turn is position: sticky so it stays reachable mid-scroll', () => {
    const css = turnsCss();
    // First definition of .turn .copy-turn (later overrides for hover/
    // touch shouldn't redeclare position).  Match from the selector to
    // the next `}` so the assertion targets that block specifically.
    const block = css.match(/\.turn \.copy-turn \{[^}]*\}/);
    expect(block, '.turn .copy-turn selector must exist in turns.css').toBeTruthy();
    expect(block[0]).toMatch(/position:\s*sticky/);
    expect(block[0]).toMatch(/top:/);
    // Background keeps the sticky button readable over scrolling prose.
    expect(block[0]).toMatch(/background:/);
  });

  it('.turn .who is no longer sticky — only the copy button pins', () => {
    // Regression: the original attempt made `.who` sticky, which
    // dragged the whole header bar across the prose.  Lock that out.
    const css = turnsCss();
    const block = css.match(/\.turn \.who \{[^}]*\}/);
    expect(block, '.turn .who selector must exist in turns.css').toBeTruthy();
    expect(block[0]).not.toMatch(/position:\s*sticky/);
  });
});

// A bash-kind tool with a couple of seed lines.  Renders <BashPanel>
// as the expanded body.
const bashTool = () => ({
  name: 'bash',
  icon: 'B',
  sig: 'echo hi',
  dur: '12ms',
  exit: 'ok',
  status: 'done',
  kind: 'bash',
  body: {
    lines: [
      { c: 'p', t: '$ echo hi' },
      { c: 'c', t: 'hi' },
    ],
    exit_code: 0,
    duration_ms: 12,
  },
});

describe('UX: inline tool block — chip expands the panel body in place', () => {
  it('chip header always paints; body only when expanded', () => {
    const { container, rerender } = render(
      <ToolBlock tool={bashTool()} toolRef="c-1-bash-1" expanded={false} onToggle={() => {}}/>
    );
    expect(container.querySelector('.toolblock')).toBeTruthy();
    expect(container.querySelector('.toolchip')).toBeTruthy();
    expect(container.querySelector('.toolblock-body'), 'body must NOT render while collapsed').toBeNull();
    // The disclosure label reads 'open' while collapsed.
    expect(container.querySelector('.toolchip .open .lbl').textContent).toBe('open');
    expect(container.querySelector('.toolchip').getAttribute('aria-expanded')).toBe('false');

    rerender(<ToolBlock tool={bashTool()} toolRef="c-1-bash-1" expanded={true} onToggle={() => {}}/>);
    expect(container.querySelector('.toolblock.expanded')).toBeTruthy();
    expect(container.querySelector('.toolblock-body'), 'body must render once expanded').toBeTruthy();
    expect(container.querySelector('.toolchip .open .lbl').textContent).toBe('hide');
    expect(container.querySelector('.toolchip').getAttribute('aria-expanded')).toBe('true');
    // The actual kind-specific body — bash output — renders inside.
    expect(container.querySelector('.toolblock-body .term')).toBeTruthy();
    expect(container.textContent).toContain('echo hi');
  });

  it('clicking the chip fires onToggle (single chip, not the body)', () => {
    let calls = 0;
    const { container } = render(
      <ToolBlock tool={bashTool()} toolRef="c-1-bash-1" expanded={false} onToggle={() => { calls += 1; }}/>
    );
    fireEvent.click(container.querySelector('.toolchip'));
    expect(calls).toBe(1);
  });

  it('expanded body exposes copy input/output and collapse controls', () => {
    let calls = 0;
    const { container } = render(
      <ToolBlock tool={bashTool()} toolRef="c-1-bash-1" expanded={true} onToggle={() => { calls += 1; }}/>
    );
    expect(container.querySelector('.toolblock-actions')).toBeTruthy();
    expect(container.textContent).toContain('input');
    expect(container.textContent).toContain('output');
    fireEvent.click([...container.querySelectorAll('.toolblock-actions button')].find(b => b.textContent.includes('collapse')));
    expect(calls).toBe(1);
  });

  it('a tool with no kind falls back to FallbackPanel inside the inline body', () => {
    const tool = { ...bashTool(), kind: 'fallback', body: { text: 'plain text payload' } };
    const { container } = render(
      <ToolBlock tool={tool} toolRef="c-1-fallback-1" expanded={true} onToggle={() => {}}/>
    );
    expect(container.querySelector('.toolblock-body .fallback-body')).toBeTruthy();
    expect(container.textContent).toContain('plain text payload');
  });

  it('expanded toolblock carries data-tool-ref for deep-link addressing', () => {
    // The hash route `#/c/<id>/t/<ref>` flips this block to expanded; a
    // future scroll-into-view feature can rely on the attribute as a
    // stable selector.
    const { container } = render(
      <ToolBlock tool={bashTool()} toolRef="c-1-bash-7" expanded={true} onToggle={() => {}}/>
    );
    const node = container.querySelector('[data-tool-ref="c-1-bash-7"]');
    expect(node).toBeTruthy();
    expect(node.classList.contains('toolblock')).toBe(true);
  });
});

describe('UX: live run status strip', () => {
  it('renders phase, tool, elapsed time, cancel, and jump controls', () => {
    const onCancel = vi.fn();
    const onJump = vi.fn();
    const { container } = render(
      <RunStatusStrip
        phase="tool"
        tname="bash"
        startedAt={Date.now() - 5000}
        onCancel={onCancel}
        onJump={onJump}
        onCollapseAll={() => {}}/>
    );
    expect(container.textContent).toContain('running tool');
    expect(container.textContent).toContain('bash');
    expect(container.textContent).toMatch(/0:0[4-6]/);
    fireEvent.click(container.querySelector('button[title="Jump to latest"]'));
    fireEvent.click(container.querySelector('button[title="Cancel run"]'));
    expect(onJump).toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalled();
  });
});
