// security-harness-panel.test.jsx
//
// Pins the parser the SecurityHarnessPanel uses to extract stage state
// from the live tool body text.  The backend emits checkpoint events
// like "security_engineer: <stage>" between stages, and those messages
// accumulate in the tool's body.text via the onCheckpoint callback in
// app.jsx.  The panel's job is to make sense of that running log
// without any backend changes — these tests pin the contract.

import { describe, it, expect } from 'vitest';
import { parseHarnessState, HARNESS_STAGES, SecurityHarnessPanel } from '../components/panels.jsx';
import { render, screen } from '@testing-library/react';
import React from 'react';

describe('parseHarnessState', () => {
  it('detects each stage as the latest mention', () => {
    const text = [
      'security_engineer: resume checkpoint sec-1780812345-7',
      'security_engineer: recon',
      'security_engineer: hunt',
    ].join('\n');
    const s = parseHarnessState(text, true);
    expect(s.lastStage).toBe('hunt');
    expect(s.runId).toBe('sec-1780812345-7');
    expect(s.resumed).toBe(true);
    expect(s.completed).toBe(false);
  });

  it('marks all earlier stages done when running mid-pipeline', () => {
    const text = 'security_engineer: validate';
    const s = parseHarnessState(text, true);
    const idx = HARNESS_STAGES.indexOf('validate');
    for (let i = 0; i < idx; i++) {
      expect(s.stageStatus[i]).toBe('done');
    }
    expect(s.stageStatus[idx]).toBe('running');
    for (let i = idx + 1; i < HARNESS_STAGES.length; i++) {
      expect(s.stageStatus[i]).toBe('pending');
    }
  });

  it('marks all stages done when the run completed', () => {
    const text = [
      'security_engineer: hunt',
      'security_engineer: completed sec-x in 312s',
    ].join('\n');
    const s = parseHarnessState(text, false);
    expect(s.completed).toBe(true);
    expect(s.stageStatus.every(st => st === 'done')).toBe(true);
  });

  it('flags the currently-running stage even when text is sparse', () => {
    const text = 'security_engineer: recon';
    const s = parseHarnessState(text, true);
    expect(s.stageStatus[0]).toBe('running');
  });

  it('renders the running stage as "done" when isRunning=false (stopped mid-stage)', () => {
    // E.g. validate parse-failed: tool stopped, last seen stage is
    // validate.  We render validate as "done" (technically: "stopped at"),
    // not "running" — the UI can pair this with an error banner.
    const text = 'security_engineer: validate';
    const s = parseHarnessState(text, false);
    const idx = HARNESS_STAGES.indexOf('validate');
    expect(s.stageStatus[idx]).toBe('done');
  });

  it('returns -1 currentIdx safely when no stage line seen yet', () => {
    const s = parseHarnessState('', true);
    expect(s.lastStage).toBe(null);
    expect(s.stageStatus.every(st => st === 'pending')).toBe(true);
  });

  it('extracts the first sec- run id encountered and keeps it stable', () => {
    const text = [
      'security_engineer: starting checkpoint sec-aaa',
      'security_engineer: recon',
      // A later mention of a different id should NOT replace the first
      // (this would only happen on a buggy backend; pinning the
      // deterministic behavior).
      'security_engineer: somehow sec-bbb',
    ].join('\n');
    const s = parseHarnessState(text, true);
    expect(s.runId).toBe('sec-aaa');
  });
});

describe('SecurityHarnessPanel rendering', () => {
  it('renders all eight stage labels in the progress bar', () => {
    const body = { text: 'security_engineer: hunt', children: [], summary: '' };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    const t = container.textContent || '';
    for (const label of ['Recon', 'Hunt', 'Validate', 'Gapfill', 'Dedupe', 'Trace', 'Feedback', 'Report']) {
      expect(t, `stage bar should include ${label}`).toContain(label);
    }
  });

  it('shows the run id in the header', () => {
    const body = {
      text: 'security_engineer: resume checkpoint sec-1780830172-2\nsecurity_engineer: hunt',
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('sec-1780830172-2');
  });

  it('surfaces the resumed badge when resume was detected', () => {
    const body = {
      text: 'security_engineer: resume checkpoint sec-x',
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('resumed');
  });

  it('renders the error banner when errorText is supplied', () => {
    const body = { text: 'security_engineer: validate', children: [] };
    const { container } = render(<SecurityHarnessPanel
      body={body}
      running={false}
      errorText="validate failed: no JSON object found in stage output"/>);
    expect(container.textContent).toContain('no JSON object found');
  });

  it('shows the completed badge when the run completed cleanly', () => {
    const body = {
      text: 'security_engineer: recon\nsecurity_engineer: completed sec-x in 99s',
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={false}/>);
    expect(container.textContent).toContain('completed');
  });

  it('falls back to "no run id yet" before any sec- appears', () => {
    const body = { text: '', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('no run id yet');
  });
});
