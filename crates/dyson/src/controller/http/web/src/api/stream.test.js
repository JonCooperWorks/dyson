import { describe, it, expect, vi } from 'vitest';
import { parseStreamEvent, dispatchStreamEvent } from './stream.js';

describe('parseStreamEvent', () => {
  it('parses a well-formed JSON event', () => {
    expect(parseStreamEvent('{"type":"text","delta":"hi"}')).toEqual({ type: 'text', delta: 'hi' });
  });

  it('returns null on malformed JSON', () => {
    expect(parseStreamEvent('not-json')).toBeNull();
    expect(parseStreamEvent('')).toBeNull();
  });

  it('returns null when the payload is not an object', () => {
    expect(parseStreamEvent('42')).toBeNull();
    expect(parseStreamEvent('"text"')).toBeNull();
    expect(parseStreamEvent('null')).toBeNull();
  });

  it('returns null when type is missing or wrong shape', () => {
    expect(parseStreamEvent('{"delta":"x"}')).toBeNull();
    expect(parseStreamEvent('{"type":42}')).toBeNull();
  });

  it('ignores non-string raw input', () => {
    expect(parseStreamEvent(null)).toBeNull();
    expect(parseStreamEvent(undefined)).toBeNull();
    expect(parseStreamEvent({ type: 'text' })).toBeNull();
  });
});

describe('dispatchStreamEvent', () => {
  it('text → onText(delta)', () => {
    const onText = vi.fn();
    dispatchStreamEvent({ type: 'text', delta: 'hi' }, { onText });
    expect(onText).toHaveBeenCalledWith('hi');
  });

  it('thinking → onThinking(delta)', () => {
    const onThinking = vi.fn();
    dispatchStreamEvent({ type: 'thinking', delta: 'mm' }, { onThinking });
    expect(onThinking).toHaveBeenCalledWith('mm');
  });

  it('tool_start → onToolStart(full msg)', () => {
    const onToolStart = vi.fn();
    const msg = { type: 'tool_start', id: 't1', name: 'bash' };
    dispatchStreamEvent(msg, { onToolStart });
    expect(onToolStart).toHaveBeenCalledWith(msg);
  });

  it('tool_result → onToolResult(full msg)', () => {
    const onToolResult = vi.fn();
    const msg = { type: 'tool_result', content: 'ok', is_error: false, view: { kind: 'bash' } };
    dispatchStreamEvent(msg, { onToolResult });
    expect(onToolResult).toHaveBeenCalledWith(msg);
  });

  it('checkpoint → onCheckpoint(full msg)', () => {
    const onCheckpoint = vi.fn();
    const msg = { type: 'checkpoint', text: 'step' };
    dispatchStreamEvent(msg, { onCheckpoint });
    expect(onCheckpoint).toHaveBeenCalledWith(msg);
  });

  it('file → onFile(full msg)', () => {
    const onFile = vi.fn();
    const msg = { type: 'file', name: 'x.png', mime_type: 'image/png', url: '/f/1', inline_image: true };
    dispatchStreamEvent(msg, { onFile });
    expect(onFile).toHaveBeenCalledWith(msg);
  });

  it('artefact → onArtefact(full msg)', () => {
    const onArtefact = vi.fn();
    const msg = { type: 'artefact', id: 'a1', kind: 'image', title: 't', url: '/a/1', bytes: 10 };
    dispatchStreamEvent(msg, { onArtefact });
    expect(onArtefact).toHaveBeenCalledWith(msg);
  });

  it('llm_error → onError(message only)', () => {
    const onError = vi.fn();
    dispatchStreamEvent({ type: 'llm_error', message: 'kaput' }, { onError });
    expect(onError).toHaveBeenCalledWith('kaput');
  });

  it('done → onDone()', () => {
    const onDone = vi.fn();
    dispatchStreamEvent({ type: 'done' }, { onDone });
    expect(onDone).toHaveBeenCalledWith();
  });

  it('returns false on unknown type and does not throw', () => {
    expect(dispatchStreamEvent({ type: 'mystery' }, {})).toBe(false);
  });

  it('missing callbacks are tolerated (no-op)', () => {
    expect(() => dispatchStreamEvent({ type: 'text', delta: 'x' }, {})).not.toThrow();
  });

  it('null msg or callbacks is a no-op that returns false', () => {
    expect(dispatchStreamEvent(null, {})).toBe(false);
    expect(dispatchStreamEvent({ type: 'text' }, null)).toBe(false);
  });
});
