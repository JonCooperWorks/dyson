// Real render tests for SecurityReportView — the findings-entity reader
// that replaces the flat markdown render when a security_review artefact
// carries metadata.report_path.  Mounts under jsdom with a mocked fetch;
// every failure mode must degrade to the caller-supplied fallback.

import React from 'react';
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, fireEvent, cleanup, act, waitFor } from '@testing-library/react';
import { SecurityReportView } from '../components/security-report.jsx';

const DOC = {
  schema_version: 1,
  run_id: 'sec-1751000000-1234',
  target: { repo_path: '/repo/vuln-demo', git_ref: 'abc123' },
  scope: 'whole repo',
  model: { provider: 'openrouter', model: 'claude-opus-4-7' },
  harness_version: 'v3',
  report_source: 'valid',
  summary: { critical: 1, high: 1, medium: 0, low: 1, new: 2, recurring: 1 },
  findings: [
    {
      id: 'F1', run_finding_id: 'F1', key: 'DYS-1A2B3C4D', recurring: true, occurrences: 4,
      title: 'Command injection in ping handler', severity: 'critical',
      vulnerability_class: 'injection_unsafe_execution',
      trust_boundary: 'remote attacker to host shell',
      entry_point: 'POST /ping', sink_or_decision: 'app.py:27 os.system',
      root_cause: 'user input concatenated into a shell command',
      affected_paths: ['vuln-demo/app.py:27'],
      evidence: ['os.system("ping " + host)'],
      reachability: 'reachable', tenant_or_instance_impact: 'host compromise',
      severity_rationale: 'unauthenticated RCE', fix_recommendation: 'use subprocess list argv',
      suggested_patch: '--- a/app.py\n+++ b/app.py\n@@\n-os.system("ping " + host)\n+subprocess.run(["ping", host])',
    },
    {
      id: 'F2', run_finding_id: 'F2', key: 'DYS-9F8E7D6C', recurring: false, occurrences: 1,
      title: 'IDOR on user lookup', severity: 'high',
      vulnerability_class: 'auth_authorization',
      trust_boundary: 'any caller to user store',
      entry_point: 'GET /users/:id', sink_or_decision: 'app.py:14 row fetch',
      root_cause: 'no ownership check', affected_paths: ['vuln-demo/app.py:14'],
      evidence: [], reachability: 'reachable', tenant_or_instance_impact: '',
      severity_rationale: '', fix_recommendation: 'scope query to session user',
      suggested_patch: '',
    },
    {
      id: 'F3', run_finding_id: 'F3', key: '', recurring: false, occurrences: 0,
      title: 'Verbose error pages', severity: 'info',
      vulnerability_class: 'information_disclosure',
      trust_boundary: '', entry_point: '', sink_or_decision: '',
      root_cause: 'debug mode left on', affected_paths: [],
      evidence: [], reachability: '', tenant_or_instance_impact: '',
      severity_rationale: '', fix_recommendation: '', suggested_patch: '',
    },
  ],
  rejected_candidates: [], gaps: [], class_coverage: [], stage_history: [],
};

const FALLBACK = <div data-testid="md-fallback">markdown body</div>;

function mockFetch(impl) {
  const fn = vi.fn(impl);
  vi.stubGlobal('fetch', fn);
  return fn;
}

function okEnvelope(doc) {
  return {
    ok: true,
    json: async () => ({ path: 'kb/security-harness/reports/x.json', content: JSON.stringify(doc) }),
  };
}

async function renderView(doc = DOC) {
  mockFetch(async () => okEnvelope(doc));
  let utils;
  await act(async () => {
    utils = render(
      <SecurityReportView reportPath="kb/security-harness/reports/x.json" fallback={FALLBACK}/>
    );
  });
  return utils;
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe('SecurityReportView — cards', () => {
  it('groups findings by severity with DYS keys and recurring badge', async () => {
    const { container, getByText, queryByTestId } = await renderView();
    expect(queryByTestId('md-fallback')).toBeNull();
    // Group order: critical before high before low (info folds into low).
    const eyebrows = [...container.querySelectorAll('.secrep-group > .eyebrow')]
      .map(e => e.textContent);
    expect(eyebrows[0]).toContain('critical');
    expect(eyebrows[1]).toContain('high');
    expect(eyebrows[2]).toContain('low');
    expect(getByText('DYS-1A2B3C4D')).toBeTruthy();
    expect(getByText('recurring x4')).toBeTruthy();
    // F3 has no ledger key — no empty chip rendered for it.
    const card3 = getByText('Verbose error pages').closest('.secrep-card');
    expect(card3.querySelectorAll('.chip').length).toBe(0);
  });

  it('expands a card to the detail fields with an escaped patch pre', async () => {
    const { container, getByText } = await renderView();
    fireEvent.click(getByText('Command injection in ping handler'));
    const body = container.querySelector('.secrep-card-body');
    expect(body).toBeTruthy();
    expect(body.textContent).toContain('user input concatenated into a shell command');
    const pres = [...body.querySelectorAll('pre.secrep-pre')];
    expect(pres.some(p => p.textContent.includes('subprocess.run(["ping", host])'))).toBe(true);
    // Model output must land as text, never as parsed HTML.
    expect(body.querySelector('.secrep-pre a, .secrep-pre code')).toBeNull();
  });

  it('severity filter and text search narrow the list', async () => {
    const { container, getByText } = await renderView();
    const sevSelect = container.querySelector('.secrep-select');
    fireEvent.change(sevSelect, { target: { value: 'high' } });
    expect(container.querySelectorAll('.secrep-card').length).toBe(1);
    expect(getByText('IDOR on user lookup')).toBeTruthy();

    fireEvent.change(sevSelect, { target: { value: '' } });
    fireEvent.change(container.querySelector('.secrep-search'), { target: { value: 'ping' } });
    const cards = [...container.querySelectorAll('.secrep-card')];
    expect(cards.length).toBe(1);
    expect(cards[0].textContent).toContain('Command injection');
  });

  it('recurring filter keeps only ledger-recurring findings', async () => {
    const { container } = await renderView();
    const flagSelect = [...container.querySelectorAll('.secrep-select')].at(-1);
    fireEvent.change(flagSelect, { target: { value: 'recurring' } });
    const cards = [...container.querySelectorAll('.secrep-card')];
    expect(cards.length).toBe(1);
    expect(cards[0].textContent).toContain('Command injection');
  });

  it('json toggle shows the raw pretty-printed doc', async () => {
    const { container, getByText } = await renderView();
    fireEvent.click(getByText('json'));
    const pre = container.querySelector('.secrep-json');
    expect(pre).toBeTruthy();
    expect(pre.textContent).toContain('"run_id": "sec-1751000000-1234"');
    expect(pre.textContent).toContain('"DYS-9F8E7D6C"');
    fireEvent.click(getByText('rendered'));
    expect(container.querySelector('.secrep-card')).toBeTruthy();
  });
});

describe('SecurityReportView — fallbacks', () => {
  it('renders the fallback on a mind-route 404', async () => {
    mockFetch(async () => ({ ok: false, status: 404, json: async () => ({}) }));
    let utils;
    await act(async () => {
      utils = render(<SecurityReportView reportPath="kb/missing.json" fallback={FALLBACK}/>);
    });
    await waitFor(() => expect(utils.queryByTestId('md-fallback')).toBeTruthy());
    expect(utils.container.querySelector('.secrep')).toBeNull();
  });

  it('renders the fallback when the doc content does not parse', async () => {
    mockFetch(async () => ({
      ok: true,
      json: async () => ({ path: 'x', content: '{"findings": [tru' }),
    }));
    let utils;
    await act(async () => {
      utils = render(<SecurityReportView reportPath="kb/torn.json" fallback={FALLBACK}/>);
    });
    await waitFor(() => expect(utils.queryByTestId('md-fallback')).toBeTruthy());
  });

  it('renders the fallback when the fetch itself rejects', async () => {
    mockFetch(async () => { throw new Error('network down'); });
    let utils;
    await act(async () => {
      utils = render(<SecurityReportView reportPath="kb/x.json" fallback={FALLBACK}/>);
    });
    await waitFor(() => expect(utils.queryByTestId('md-fallback')).toBeTruthy());
  });
});
