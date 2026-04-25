// Tests for the streaming SSE handlers that power the live subagent
// panel.  Pins the contract laid out in
// `controller::http::SubagentEventBus`: nested events tagged with
// `parent_tool_id` attach to the parent tool's body.children list
// and never appear as new top-level chips.
//
// These exist as a regression net for the original "empty box until
// the subagent finishes" bug — without them, a future refactor that
// drops parent_tool_id from the dispatch would silently send the UI
// back to looking dead during a subagent run.

import { describe, it, expect, beforeEach } from 'vitest';

import { streamCallbacks } from '../components/app.jsx';
import { app, __resetAppStoreForTests } from '../store/app.js';
import { ensureSession, __resetSessionsForTests } from '../store/sessions.js';

beforeEach(() => {
  __resetAppStoreForTests();
  __resetSessionsForTests();
});

const tools = () => app.getSnapshot().tools;

describe('onToolStart — top-level vs nested', () => {
  it('top-level tool_start mints a tool entry and appends a transcript block', () => {
    ensureSession('c-0001');
    streamCallbacks('c-0001').onToolStart({ id: 'parent_42', name: 'security_engineer' });
    const t = tools()['parent_42'];
    expect(t).toBeDefined();
    expect(t.name).toBe('security_engineer');
    expect(t.status).toBe('running');
    // It does NOT pre-flip to 'subagent' — that happens once the
    // first nested tool_start arrives, so a subagent that runs
    // without inner tool calls still falls back to the plain
    // FallbackPanel rather than rendering an empty list.
    expect(t.kind).not.toBe('subagent');
  });

  it('nested tool_start with parent_tool_id attaches a child to the parent panel', () => {
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'parent_42', name: 'security_engineer' });
    cb.onToolStart({ id: 'inner_1', name: 'bash', parent_tool_id: 'parent_42' });

    const parent = tools()['parent_42'];
    expect(parent.kind).toBe('subagent');
    expect(parent.body.children).toHaveLength(1);
    expect(parent.body.children[0]).toMatchObject({
      id: 'inner_1', name: 'bash', status: 'running',
    });

    // Critically: no top-level entry was minted for the inner call.
    expect(tools()['inner_1']).toBeUndefined();
  });

  it('multiple nested tool_starts accumulate as children in arrival order', () => {
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'parent_42', name: 'orchestrator' });
    cb.onToolStart({ id: 'a', name: 'bash', parent_tool_id: 'parent_42' });
    cb.onToolStart({ id: 'b', name: 'read_file', parent_tool_id: 'parent_42' });
    cb.onToolStart({ id: 'c', name: 'search_files', parent_tool_id: 'parent_42' });

    const children = tools()['parent_42'].body.children;
    expect(children.map(c => c.id)).toEqual(['a', 'b', 'c']);
    expect(children.map(c => c.name)).toEqual(['bash', 'read_file', 'search_files']);
  });

  it('nested tool_start with no matching parent is a no-op (race on reload)', () => {
    ensureSession('c-0001');
    streamCallbacks('c-0001').onToolStart({
      id: 'inner_1', name: 'bash', parent_tool_id: 'ghost_parent',
    });
    // No tools entry should appear; the call drops on the floor
    // rather than minting a phantom panel.
    expect(Object.keys(tools())).toHaveLength(0);
  });
});

describe('onToolResult — top-level vs nested', () => {
  it('nested tool_result patches the matching child by tool_use_id', () => {
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'parent_42', name: 'coder' });
    cb.onToolStart({ id: 'inner_1', name: 'bash', parent_tool_id: 'parent_42' });
    cb.onToolStart({ id: 'inner_2', name: 'edit_file', parent_tool_id: 'parent_42' });

    cb.onToolResult({
      content: 'inner one ok',
      is_error: false,
      view: { kind: 'bash', lines: [], exit_code: 0, duration_ms: 42 },
      parent_tool_id: 'parent_42',
      tool_use_id: 'inner_1',
    });

    const children = tools()['parent_42'].body.children;
    const first = children.find(c => c.id === 'inner_1');
    const second = children.find(c => c.id === 'inner_2');
    expect(first).toMatchObject({ status: 'done', exit: 'ok', kind: 'bash' });
    expect(second).toMatchObject({ status: 'running' });
  });

  it('parallel nested calls are routed correctly by tool_use_id', () => {
    // The frontend cannot rely on liveToolRef ordering when a
    // subagent dispatches calls in parallel — each must land in the
    // right child entry by id, not by recency.
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'parent_42', name: 'orchestrator' });
    cb.onToolStart({ id: 'a', name: 'bash', parent_tool_id: 'parent_42' });
    cb.onToolStart({ id: 'b', name: 'bash', parent_tool_id: 'parent_42' });

    // Results arrive out of order — `b` finishes first.
    cb.onToolResult({
      content: 'b done', is_error: false, parent_tool_id: 'parent_42', tool_use_id: 'b',
    });
    const after_b = tools()['parent_42'].body.children;
    expect(after_b.find(c => c.id === 'b').status).toBe('done');
    expect(after_b.find(c => c.id === 'a').status).toBe('running');

    cb.onToolResult({
      content: 'a errored', is_error: true, parent_tool_id: 'parent_42', tool_use_id: 'a',
    });
    const after_a = tools()['parent_42'].body.children;
    expect(after_a.find(c => c.id === 'a')).toMatchObject({ status: 'done', exit: 'err' });
  });

  it('parent ToolResult preserves children when the panel is in subagent mode', () => {
    // The bug this guards against: applyToolView would otherwise
    // overwrite body with `{ text: content }` and wipe out the live
    // children list users have been watching for minutes.
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'parent_42', name: 'security_engineer' });
    cb.onToolStart({ id: 'inner_1', name: 'bash', parent_tool_id: 'parent_42' });
    cb.onToolResult({
      content: 'shell ok', is_error: false,
      parent_tool_id: 'parent_42', tool_use_id: 'inner_1',
    });

    // Now the parent finishes — its content arrives at top level.
    cb.onToolResult({ content: 'Final report:\n\n# Findings…', is_error: false });

    const parent = tools()['parent_42'];
    expect(parent.kind).toBe('subagent');
    expect(parent.body.children).toHaveLength(1);
    expect(parent.body.children[0].id).toBe('inner_1');
    expect(parent.body.summary).toContain('Final report');
    expect(parent.status).toBe('done');
  });

  it('top-level tool_result still works for non-subagent tools', () => {
    ensureSession('c-0001');
    const cb = streamCallbacks('c-0001');
    cb.onToolStart({ id: 'bash_42', name: 'bash' });
    cb.onToolResult({
      content: 'ok',
      is_error: false,
      view: { kind: 'bash', lines: [], exit_code: 0, duration_ms: 10 },
    });
    const t = tools()['bash_42'];
    expect(t.kind).toBe('bash');
    expect(t.status).toBe('done');
  });
});
