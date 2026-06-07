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
});
