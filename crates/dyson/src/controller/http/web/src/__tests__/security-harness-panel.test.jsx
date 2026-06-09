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

// Build a `<!-- security-harness-state {...} -->` block exactly the way
// the backend bakes it: JSON.stringify the payload and escape every `>`
// so the closing `-->` is unambiguous.  See
// `security_engineer/mod.rs::bake_panel_state_snapshot` for the
// authoritative producer this mirrors.
function snapshotBlock(payload) {
  const safe = JSON.stringify(payload).replace(/>/g, '\\u003e');
  return `<!-- security-harness-state ${safe} -->`;
}

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

  // ---- Phase 2 behaviors --------------------------------------------------

  it('auto-derives error banner from body.text when exit=err', () => {
    const body = {
      text: 'security_engineer: validate failed: no JSON object found in stage output',
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} exit="err" running={false}/>);
    expect(container.textContent).toContain('no JSON object found');
    expect(container.textContent).toContain('failed at Validate');
  });

  it('falls back to a generic message when exit=err but no failure line was captured', () => {
    const body = { text: 'security_engineer: starting checkpoint sec-x', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} exit="err" running={false}/>);
    expect(container.textContent).toContain('Harness returned an error');
  });

  it('renders the findings counter when a findings line is in the stream', () => {
    const body = {
      text: [
        'security_engineer: hunt',
        'security_engineer: findings critical=1 high=20 medium=48 low=47',
      ].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toMatch(/116 findings/);
    expect(container.textContent).toContain('critical');
    expect(container.textContent).toContain('high');
  });

  it('hides the findings counter when total is zero', () => {
    const body = { text: 'security_engineer: recon', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).not.toMatch(/\d+ findings?/);
  });

  it('renders the class coverage grid when class hunt outcomes appear', () => {
    const body = {
      text: [
        'security_engineer: hunt',
        'security_engineer: hunt: class auth_authorization hunted (3 findings)',
        'security_engineer: hunt: class session_oauth_csrf cleared',
        'security_engineer: hunt: class frontend_security_ux inapplicable',
      ].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('Class coverage (3/24 reported)');
    expect(container.textContent).toContain('auth_authorization');
    expect(container.textContent).toContain('session_oauth_csrf');
    expect(container.textContent).toContain('frontend_security_ux');
  });

  it('shows the "failed at <stage>" badge with the right stage label', () => {
    const body = {
      text: [
        'security_engineer: hunt',
        'security_engineer: validate',
        'security_engineer: validate failed: parse error',
      ].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} exit="err" running={false}/>);
    expect(container.textContent).toContain('failed at Validate');
  });
});

describe('parseHarnessState — Phase 2 fields', () => {
  it('extracts a per-class findings count from a hunt summary line', () => {
    const text = 'security_engineer: hunt: class auth_authorization hunted (5 findings)';
    const s = parseHarnessState(text, true);
    expect(s.classStatus.auth_authorization).toEqual({ status: 'hunted', count: 5 });
  });

  it('extracts cleared and inapplicable status without a count', () => {
    const text = [
      'security_engineer: hunt: class session_oauth_csrf cleared',
      'security_engineer: hunt: class frontend_security_ux inapplicable',
    ].join('\n');
    const s = parseHarnessState(text, true);
    expect(s.classStatus.session_oauth_csrf).toEqual({ status: 'cleared', count: 0 });
    expect(s.classStatus.frontend_security_ux).toEqual({ status: 'inapplicable', count: 0 });
  });

  it('sums up findings_by_severity from the `findings` summary line', () => {
    const text = 'security_engineer: findings critical=1 high=20 medium=48 low=47';
    const s = parseHarnessState(text, true);
    expect(s.findings).toEqual({ critical: 1, high: 20, medium: 48, low: 47 });
    expect(s.totalFindings).toBe(116);
  });

  it('marks failedAtStage from a `<stage> failed:` line', () => {
    const text = 'security_engineer: validate failed: no JSON object found';
    const s = parseHarnessState(text, false, true);
    expect(s.failedAtStage).toBe('validate');
    expect(s.failureMessage).toContain('no JSON object');
    expect(s.errored).toBe(true);
  });

  it('marks the failed stage as "errored", not "done"', () => {
    const text = [
      'security_engineer: recon',
      'security_engineer: hunt',
      'security_engineer: validate',
      'security_engineer: validate failed: parse error',
    ].join('\n');
    const s = parseHarnessState(text, false, true);
    const idx = HARNESS_STAGES.indexOf('validate');
    expect(s.stageStatus[idx]).toBe('errored');
    expect(s.stageStatus[0]).toBe('done'); // recon
    expect(s.stageStatus[1]).toBe('done'); // hunt
  });

  it('captures a bare `error` line as failureMessage when no stage-failed line is present', () => {
    const text = 'security_engineer: error (4072940ms)';
    const s = parseHarnessState(text, false, true);
    expect(s.failureMessage).toContain('4072940');
    expect(s.errored).toBe(true);
  });

  // ---- Phase 4: HTML-comment-wrapped event block survives rehydrate ------

  it('parses checkpoint events out of the HTML-comment block baked into tool content', () => {
    // The backend wraps the CheckpointEvent stream in
    //   <!-- security-harness-events
    //   security_engineer: <line>
    //   security_engineer: <line>
    //   -->
    // and prepends it to the tool's content.  On rehydrate from the
    // conversation API, body.text gets set to this content — the
    // parser must find the event lines inside the comment.
    const text = [
      '<!-- security-harness-events',
      'security_engineer: resuming checkpoint sec-1780830172-2',
      'security_engineer: validate',
      'security_engineer: validate failed: no JSON object found',
      'security_engineer: completed sec-1780830172-2 in 134s',
      '-->',
      '',
      '# Security Review: vllm/distributed',
      '',
      '## CRITICAL',
      'No findings.',
    ].join('\n');
    const s = parseHarnessState(text, false, false);
    expect(s.runId).toBe('sec-1780830172-2');
    expect(s.resumed).toBe(true);
    expect(s.completed).toBe(true);
    expect(s.failedAtStage).toBe('validate');
    expect(s.failureMessage).toContain('no JSON object found');
  });

  it('still handles the live (uncommented) stream the same way after rehydrate baking', () => {
    // Belt-and-braces: a live run's body.text uses bare lines (no
    // comment wrapper because onCheckpoint just appends each message
    // verbatim).  Verify the same parser handles both shapes.
    const live = [
      'security_engineer: starting checkpoint sec-aaa',
      'security_engineer: recon',
      'security_engineer: hunt',
    ].join('\n');
    const liveState = parseHarnessState(live, true);
    expect(liveState.runId).toBe('sec-aaa');
    expect(liveState.lastStage).toBe('hunt');

    // Same events, comment-wrapped (rehydrate-shaped)
    const wrapped = ['<!-- security-harness-events', ...live.split('\n'), '-->'].join('\n');
    const wrappedState = parseHarnessState(wrapped, true);
    expect(wrappedState.runId).toBe('sec-aaa');
    expect(wrappedState.lastStage).toBe('hunt');
  });

  // ---- Phase 5: structured panel-state snapshot wins on rehydrate -------
  //
  // The 2026-06-08 QA observed that after a hard refresh, the panel came
  // back showing 7 ✓ + 1 ▸ Report, `(no run id yet)`, no findings counter,
  // and no class grid — even though the persisted tool content carried
  // all 62 checkpoint events.  The events bake survived rehydrate but the
  // event-to-state derivation was lossy in a way that depended on the
  // hydrate / SSE replay / applyToolView ordering.  These tests pin the
  // contract that the structured snapshot is now the source of truth.

  it('rehydrates the full final panel state from the snapshot block alone', () => {
    // No event lines at all — only the snapshot.  The panel must still
    // render run-id, completed=true, findings rollup, and class grid.
    // This is the regression test for the c-0016 symptom (body.text
    // had events but lost everything except stage glyphs on refresh):
    // the same UI now reconstructs from the JSON snapshot directly.
    const text = [
      snapshotBlock({
        run_id: 'sec-1780939724-2',
        completed: true,
        failed_at_stage: null,
        failure_message: null,
        findings: { critical: 18, high: 19, medium: 15, low: 3 },
        class_status: {
          injection_unsafe_execution: { status: 'hunted', count: 5 },
          secrets_credentials: { status: 'hunted', count: 5 },
          ssrf_outbound_network: { status: 'cleared', count: 0 },
          container_sandbox_runtime: { status: 'inapplicable', count: 0 },
        },
      }),
      '',
      '# Security Review',
      '## CRITICAL',
      'finding-001: SQL Injection in login()',
    ].join('\n');
    const s = parseHarnessState(text, false, false);
    expect(s.runId).toBe('sec-1780939724-2');
    expect(s.completed).toBe(true);
    expect(s.stageStatus.every(st => st === 'done')).toBe(true);
    expect(s.findings).toEqual({ critical: 18, high: 19, medium: 15, low: 3 });
    expect(s.totalFindings).toBe(55);
    expect(s.classStatus.injection_unsafe_execution).toEqual({ status: 'hunted', count: 5 });
    expect(s.classStatus.ssrf_outbound_network).toEqual({ status: 'cleared', count: 0 });
    expect(s.classStatus.container_sandbox_runtime).toEqual({ status: 'inapplicable', count: 0 });
  });

  it('snapshot wins over inconsistent event lines (the c-0016 reproducer)', () => {
    // Event stream lost the `findings` and `completed` lines AND the
    // `created checkpoint sec-...` line (the exact subset that survived
    // the 2026-06-08 refresh) — but the snapshot carries the truth.
    // Without this preference, the panel would render mid-pipeline.
    const text = [
      snapshotBlock({
        run_id: 'sec-1780939724-2',
        completed: true,
        findings: { critical: 18, high: 19, medium: 15, low: 3 },
        class_status: { injection_unsafe_execution: { status: 'hunted', count: 5 } },
      }),
      '<!-- security-harness-events',
      'security_engineer: recon',
      'security_engineer: hunt',
      'security_engineer: validate',
      'security_engineer: gapfill',
      'security_engineer: dedupe',
      'security_engineer: trace',
      'security_engineer: feedback',
      'security_engineer: report',
      '-->',
    ].join('\n');
    // Even with running=true and no `completed` line in the events,
    // the snapshot forces completed=true and the StageBar fully ticks.
    const s = parseHarnessState(text, true, false);
    expect(s.runId).toBe('sec-1780939724-2');
    expect(s.completed).toBe(true);
    expect(s.stageStatus.every(st => st === 'done')).toBe(true);
    expect(s.totalFindings).toBe(55);
  });

  it('renders an `errored at <stage>` panel from a failure snapshot', () => {
    const text = snapshotBlock({
      run_id: 'sec-aaa',
      completed: false,
      failed_at_stage: 'validate',
      failure_message: 'no JSON object found in stage output',
      findings: { critical: 0, high: 0, medium: 0, low: 0 },
      class_status: {},
    });
    const s = parseHarnessState(text, false, true);
    expect(s.runId).toBe('sec-aaa');
    expect(s.completed).toBe(false);
    expect(s.failedAtStage).toBe('validate');
    expect(s.failureMessage).toBe('no JSON object found in stage output');
    expect(s.errored).toBe(true);
    const validateIdx = HARNESS_STAGES.indexOf('validate');
    expect(s.stageStatus[validateIdx]).toBe('errored');
  });

  it('falls back to event parsing when no snapshot block is present (live run)', () => {
    // A run still in progress hasn't emitted a snapshot — the bake
    // happens at return time.  The parser must keep working on the
    // bare event stream so the StageBar advances during the run.
    const text = [
      'security_engineer: created checkpoint sec-live-1',
      'security_engineer: recon',
      'security_engineer: hunt',
      'security_engineer: findings critical=2 high=3 medium=1 low=0',
    ].join('\n');
    const s = parseHarnessState(text, true, false);
    expect(s.runId).toBe('sec-live-1');
    expect(s.lastStage).toBe('hunt');
    expect(s.totalFindings).toBe(6);
    expect(s.completed).toBe(false);
  });

  it('snapshot survives a JSON payload whose failure message contains `>` and `-->`', () => {
    // The bake escapes every `>` so an attacker-supplied failure
    // message can't close the host HTML comment early.  This test
    // pins both the producer-side escaping AND the consumer-side
    // ability to recover the original string after JSON.parse
    // un-escapes it.
    const nasty = 'parse error at `--><script>` (offset 12)';
    const safe = JSON.stringify({
      run_id: 'sec-x',
      completed: false,
      failed_at_stage: 'validate',
      failure_message: nasty,
      findings: { critical: 0, high: 0, medium: 0, low: 0 },
      class_status: {},
    }).replace(/>/g, '\\u003e');
    // Sanity: the embedded JSON must NOT contain a literal `-->` after
    // escaping — otherwise the closing-comment search would terminate
    // inside the payload.
    expect(safe).not.toContain('-->');
    const text = `<!-- security-harness-state ${safe} -->`;
    const s = parseHarnessState(text, false, true);
    expect(s.failedAtStage).toBe('validate');
    expect(s.failureMessage).toBe(nasty);
  });

  it('ignores a malformed snapshot block and falls back to event parsing', () => {
    // A truncated / mangled snapshot must not poison the parse — the
    // panel should silently degrade to event parsing for live UX
    // rather than going blank.
    const text = [
      '<!-- security-harness-state {"run_id":"sec-bbb",completed:tr -->',
      'security_engineer: created checkpoint sec-aaa',
      'security_engineer: hunt',
    ].join('\n');
    const s = parseHarnessState(text, true, false);
    expect(s.runId).toBe('sec-aaa'); // fell back to event-derived id
    expect(s.lastStage).toBe('hunt');
  });
});

describe('SecurityHarnessPanel — UX visual states', () => {
  it('shows the initializing strip when the tool is running but no event has landed', () => {
    // The c-0055 screenshot case: 9 minutes in, harness alive, no
    // CheckpointEvent emitted yet (they batch at tool return).  The
    // operator was staring at all-identical stage cells with no signal
    // about whether the harness was working or stuck.  The strip
    // gives them an "alive" signal even before the first event.
    const body = { text: '', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('harness initializing');
    expect(container.textContent).toContain('loading checkpoint');
  });

  it('hides the initializing strip once any stage event has landed', () => {
    const body = { text: 'security_engineer: recon', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).not.toContain('harness initializing');
  });

  it('hides the initializing strip when the tool finished (no point telling the operator it is "initializing" after exit)', () => {
    const body = { text: '', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={false}/>);
    expect(container.textContent).not.toContain('harness initializing');
  });

  it('hides the initializing strip when the tool errored (errored badge owns the signal)', () => {
    const body = { text: '', children: [] };
    const { container } = render(<SecurityHarnessPanel
      body={body}
      running={false}
      exit="err"/>);
    expect(container.textContent).not.toContain('harness initializing');
  });

  it('prefixes each stage label with a state glyph (▸/✓/✕) so the active cell is unmistakable at a glance', () => {
    // recon done, hunt running, validate failed, rest pending.
    const body = {
      text: [
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: validate',
        'security_engineer: validate failed: parse error',
      ].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} exit="err" running={false}/>);
    const t = container.textContent || '';
    // Done cells get a checkmark; errored cells get an x.
    expect(t).toContain('✓ Recon');
    expect(t).toContain('✓ Hunt');
    expect(t).toContain('✕ Validate');
    // Pending cells have no prefix.
    expect(t).toMatch(/(?:^|[^▸✓✕ ])Gapfill/);
  });

  it('prefixes the currently-running stage with ▸', () => {
    const body = {
      text: ['security_engineer: recon', 'security_engineer: hunt'].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    expect(container.textContent).toContain('▸ Hunt');
  });

  it('renders a fully-completed panel from a snapshot block with no event lines', () => {
    // End-to-end check of the rehydrate fix: the same body shape the
    // backend bakes for a completed run.  All eight stages must show ✓,
    // the findings rollup must surface in the header, and the class
    // grid must populate from the snapshot's `class_status`.
    const body = {
      text: snapshotBlock({
        run_id: 'sec-1780939724-2',
        completed: true,
        failed_at_stage: null,
        failure_message: null,
        findings: { critical: 18, high: 19, medium: 15, low: 3 },
        class_status: {
          injection_unsafe_execution: { status: 'hunted', count: 5 },
          secrets_credentials: { status: 'hunted', count: 5 },
          crypto_randomness: { status: 'hunted', count: 2 },
        },
      }) + '\n\n# Security Review\n',
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={false}/>);
    const t = container.textContent || '';
    // All eight stages tick — the regression behaviour was ▸ Report.
    for (const label of ['Recon', 'Hunt', 'Validate', 'Gapfill', 'Dedupe', 'Trace', 'Feedback', 'Report']) {
      expect(t, `${label} should render as done`).toContain(`✓ ${label}`);
    }
    expect(t).toContain('sec-1780939724-2');
    // FindingsCounter renders the rollup row (total + per-severity).
    expect(t).toContain('55 findings');
    expect(t).toContain('18');
    // ClassGrid renders each class chip with the hunted count.
    expect(t).toContain('injection_unsafe_execution');
    expect(t).toContain('crypto_randomness');
  });
});
