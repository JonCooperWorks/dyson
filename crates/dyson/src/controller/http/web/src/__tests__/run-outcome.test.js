import { it, expect, vi } from 'vitest';
import { dispatchStreamEvent } from '../api/stream.js';
it('surfaces incomplete runs without treating them as success', () => {
  const onError = vi.fn();
  const onDone = vi.fn();
  expect(dispatchStreamEvent({ type: 'run_outcome', outcome: { status: 'iteration_limit', warnings: [] } }, { onError, onDone })).toBe(true);
  expect(onError).toHaveBeenCalledWith(expect.stringContaining('iteration_limit'));
  expect(onDone).not.toHaveBeenCalled();
});
it('completed outcomes do not generate errors', () => {
  const onError = vi.fn();
  const onRunOutcome = vi.fn();
  const outcome = { status: 'completed', warnings: [] };
  expect(dispatchStreamEvent({ type: 'run_outcome', outcome }, { onError, onRunOutcome })).toBe(true);
  expect(onError).not.toHaveBeenCalled();
  expect(onRunOutcome).toHaveBeenCalledWith(outcome);
});
