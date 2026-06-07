// One-off rendering inspection — dumps the rendered DOM for a few
// canonical fixtures so I can evaluate the visual structure of the
// SecurityHarnessPanel without spinning up a real browser.  Not a
// regression test; the assertions are intentionally loose.
//
// Run with: npm test -- --run security-harness-panel-fixture --reporter=verbose
//
// These match the actual c-0051 trace shape: recon completed, hunt
// completed, validate failed.

import { describe, it } from 'vitest';
import { SecurityHarnessPanel } from '../components/panels.jsx';
import { render } from '@testing-library/react';
import React from 'react';

function dump(label, html) {
  // eslint-disable-next-line no-console
  console.log(`\n===== ${label} =====\n${html}\n`);
}

describe('SecurityHarnessPanel — rendered structure', () => {
  it('running mid-hunt (the active-run shape)', () => {
    const body = {
      text: [
        'security_engineer: starting checkpoint sec-1780830172-2 for /var/lib/dyson/workspace/programs/vllm/vllm/distributed',
        'security_engineer: recon',
        'security_engineer: hunt',
      ].join('\n'),
      children: [
        { id: '1', name: 'read_file', status: 'done', exit: 'ok', dur: '12ms' },
        { id: '2', name: 'ast_query', status: 'done', exit: 'ok', dur: '4ms' },
        { id: '3', name: 'taint_trace', status: 'running' },
      ],
      summary: '',
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    dump('1. running mid-hunt', container.innerHTML);
  });

  it('validate failed (the c-0051 actual shape)', () => {
    const body = {
      text: [
        'security_engineer: resume checkpoint sec-1780830172-2 for /var/lib/dyson/workspace/programs/vllm/vllm/distributed',
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: validate',
      ].join('\n'),
      children: [
        { id: '1', name: 'read_file', status: 'done', exit: 'ok', dur: '12ms' },
        // imagine 600 of these...
      ],
      summary: '',
    };
    const { container } = render(<SecurityHarnessPanel
      body={body}
      running={false}
      errorText="validate stage failed: no JSON object found in stage output (same error on original run and resume)"/>);
    dump('2. validate failed (c-0051)', container.innerHTML);
  });

  it('clean completion (the happy path we have not seen yet)', () => {
    const body = {
      text: [
        'security_engineer: starting checkpoint sec-z for /scope',
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: validate',
        'security_engineer: gapfill',
        'security_engineer: dedupe',
        'security_engineer: trace',
        'security_engineer: feedback',
        'security_engineer: report',
        'security_engineer: completed sec-z in 312s',
      ].join('\n'),
      children: [],
      summary: 'Security review complete: 12 confirmed findings (3 CRITICAL, 6 HIGH, 3 MEDIUM)',
    };
    const { container } = render(<SecurityHarnessPanel
      body={body}
      running={false}
      summary={body.summary}/>);
    dump('3. clean completion', container.innerHTML);
  });

  it('empty (just started)', () => {
    const body = { text: '', children: [] };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    dump('4. empty / just started', container.innerHTML);
  });

  // ---- Phase 2 shapes -----------------------------------------------------

  it('5. mid-hunt with live class coverage + findings counter', () => {
    const body = {
      text: [
        'security_engineer: starting checkpoint sec-1780830172-2',
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: hunt: class auth_authorization hunted (3 findings)',
        'security_engineer: hunt: class ssrf_outbound_network hunted (2 findings)',
        'security_engineer: hunt: class session_oauth_csrf cleared',
        'security_engineer: hunt: class frontend_security_ux inapplicable',
        'security_engineer: hunt: class container_sandbox_runtime hunted (1 findings)',
        'security_engineer: hunt: class injection_unsafe_execution hunted (4 findings)',
        'security_engineer: findings critical=1 high=4 medium=5 low=0',
      ].join('\n'),
      children: [
        { id: '1', name: 'read_file', status: 'done', exit: 'ok', dur: '12ms' },
        { id: '2', name: 'ast_query', status: 'done', exit: 'ok', dur: '4ms' },
      ],
    };
    const { container } = render(<SecurityHarnessPanel body={body} running={true}/>);
    dump('5. mid-hunt + class grid + findings', container.innerHTML);
  });

  it('6. failed at validate (the c-0051 shape, now with auto-derived error banner)', () => {
    const body = {
      text: [
        'security_engineer: resume checkpoint sec-1780830172-2',
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: hunt: class auth_authorization hunted (5 findings)',
        'security_engineer: hunt: class ssrf_outbound_network hunted (8 findings)',
        'security_engineer: findings critical=1 high=20 medium=48 low=47',
        'security_engineer: validate',
        'security_engineer: validate failed: no JSON object found in stage output',
      ].join('\n'),
      children: [],
    };
    const { container } = render(<SecurityHarnessPanel body={body} exit="err" running={false}/>);
    dump('6. validate failed — full state + auto error banner', container.innerHTML);
  });

  it('7. completed with full findings + class coverage', () => {
    const body = {
      text: [
        'security_engineer: starting checkpoint sec-z',
        'security_engineer: recon',
        'security_engineer: hunt',
        'security_engineer: hunt: class auth_authorization hunted (3 findings)',
        'security_engineer: hunt: class ssrf_outbound_network cleared',
        'security_engineer: findings critical=1 high=3 medium=5 low=3',
        'security_engineer: validate',
        'security_engineer: gapfill',
        'security_engineer: dedupe',
        'security_engineer: trace',
        'security_engineer: feedback',
        'security_engineer: report',
        'security_engineer: completed sec-z in 312s',
      ].join('\n'),
      children: [],
      summary: 'Security review complete: 12 confirmed findings (1 CRITICAL, 3 HIGH, 5 MEDIUM, 3 LOW)',
    };
    const { container } = render(<SecurityHarnessPanel
      body={body}
      running={false}
      summary={body.summary}/>);
    dump('7. clean completion + findings + classes', container.innerHTML);
  });
});
