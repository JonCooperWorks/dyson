/* Dyson — SecurityReportView: entity cards for a security-harness run's
 * structured report document (kb/security-harness/reports/<run_id>.json,
 * fetched through the existing /api/mind/file route).
 *
 * ArtefactReader branches here when the artefact metadata carries a
 * `report_path`; every failure mode (missing doc, oversize-skipped kb
 * file, torn write, fetch error) falls back to the caller-supplied
 * markdown render so a report is never less readable than before.
 */

import React, { useState, useEffect, useMemo } from 'react';
import { copyToClipboard } from 'dyson-common-ui';
import { Icon } from './icons.jsx';
import { SEVERITY_LABELS, SEVERITY_COLOR } from './panels.jsx';

// Mirror SeverityRollup's folding (types.rs) so the cards can never
// disagree with the summary counts — except unknown severities land in
// `low` instead of being dropped: a finding must never disappear from
// the entity view because a model mistyped its severity.
function severityBucket(sev) {
  const s = String(sev || '').toLowerCase();
  if (s === 'critical' || s === 'high' || s === 'medium') return s;
  return 'low';
}

function matchesQuery(f, q) {
  if (!q) return true;
  const hay = [
    f.title, f.key, f.vulnerability_class, f.entry_point,
    f.sink_or_decision, f.root_cause,
    ...(Array.isArray(f.affected_paths) ? f.affected_paths : []),
  ].join('\n').toLowerCase();
  return hay.includes(q);
}

export function SecurityReportView({ reportPath, fallback }) {
  const [doc, setDoc] = useState(null);
  const [unavailable, setUnavailable] = useState(false);
  const [view, setView] = useState('cards');
  const [sevFilter, setSevFilter] = useState('');
  const [classFilter, setClassFilter] = useState('');
  const [flagFilter, setFlagFilter] = useState('all');
  const [query, setQuery] = useState('');
  const [expanded, setExpanded] = useState(() => new Set());
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setDoc(null);
    setUnavailable(false);
    setExpanded(new Set());
    fetch('/api/mind/file?path=' + encodeURIComponent(reportPath), { credentials: 'same-origin' })
      .then(r => { if (!r.ok) throw new Error(String(r.status)); return r.json(); })
      .then(payload => {
        // The mind route wraps the file in a JSON envelope; `content` is
        // the raw file STRING. Torn writes (Workspace::save is not
        // atomic) surface here as a parse throw → markdown fallback.
        const parsed = JSON.parse(payload.content);
        if (!parsed || !Array.isArray(parsed.findings)) throw new Error('bad doc');
        if (!cancelled) setDoc(parsed);
      })
      .catch(() => { if (!cancelled) setUnavailable(true); });
    return () => { cancelled = true; };
  }, [reportPath]);

  const classes = useMemo(() => {
    if (!doc) return [];
    const set = new Set(doc.findings.map(f => f.vulnerability_class).filter(Boolean));
    return [...set].sort();
  }, [doc]);

  if (unavailable || !doc) return fallback;

  const q = query.trim().toLowerCase();
  const visible = doc.findings
    .map((f, idx) => ({ f, idx }))
    .filter(({ f }) => !sevFilter || severityBucket(f.severity) === sevFilter)
    .filter(({ f }) => !classFilter || f.vulnerability_class === classFilter)
    .filter(({ f }) => flagFilter === 'all' || (flagFilter === 'recurring') === !!f.recurring)
    .filter(({ f }) => matchesQuery(f, q));
  const groups = SEVERITY_LABELS
    .map(sev => ({ sev, items: visible.filter(({ f }) => severityBucket(f.severity) === sev) }))
    .filter(g => g.items.length > 0);

  const toggle = (idx) => setExpanded(prev => {
    const next = new Set(prev);
    if (next.has(idx)) next.delete(idx); else next.add(idx);
    return next;
  });

  const copyJson = async () => {
    if (await copyToClipboard(JSON.stringify(doc, null, 2))) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  };

  const summary = doc.summary || {};
  return (
    <div className="secrep">
      <div className="secrep-inner">
        <div className="secrep-summary">
          <span style={{fontWeight:600, color:'var(--fg)'}}>
            {doc.findings.length} {doc.findings.length === 1 ? 'finding' : 'findings'}
          </span>
          {SEVERITY_LABELS.map(sev => {
            const n = summary[sev] || 0;
            if (!n) return null;
            return (
              <span key={sev} className="secrep-sev">
                <span className="secrep-dot" style={{background: SEVERITY_COLOR[sev]}}/>
                <span style={{color:'var(--fg)'}}>{n}</span>
                <span style={{fontVariant:'small-caps', letterSpacing:0.5}}>{sev}</span>
              </span>
            );
          })}
          <span>{summary.new || 0} new · {summary.recurring || 0} recurring</span>
          <span className="mono secrep-summary-path" title={doc.target && doc.target.repo_path}>
            {doc.target && doc.target.repo_path}
          </span>
          {doc.model && doc.model.model && <span className="mono">{doc.model.model}</span>}
        </div>

        <div className="secrep-controls">
          {view === 'cards' && (
            <>
              <select className="secrep-select" value={sevFilter} onChange={e => setSevFilter(e.target.value)}>
                <option value="">all severities</option>
                {SEVERITY_LABELS.map(s => <option key={s} value={s}>{s}</option>)}
              </select>
              {classes.length > 0 && (
                <select className="secrep-select" value={classFilter} onChange={e => setClassFilter(e.target.value)}>
                  <option value="">all classes</option>
                  {classes.map(c => <option key={c} value={c}>{c}</option>)}
                </select>
              )}
              <select className="secrep-select" value={flagFilter} onChange={e => setFlagFilter(e.target.value)}>
                <option value="all">new + recurring</option>
                <option value="new">new</option>
                <option value="recurring">recurring</option>
              </select>
              <input className="secrep-search" type="search" placeholder="search"
                     value={query} onChange={e => setQuery(e.target.value)}/>
            </>
          )}
          {view === 'json' && (
            <button className="btn xs ghost" onClick={copyJson}>{copied ? 'copied' : 'copy'}</button>
          )}
          <span style={{flex:1}}/>
          <button className="btn xs ghost secrep-toggle" data-on={view === 'cards'}
                  onClick={() => setView('cards')}>rendered</button>
          <button className="btn xs ghost secrep-toggle" data-on={view === 'json'}
                  onClick={() => setView('json')}>json</button>
        </div>

        {view === 'json' ? (
          <div className="secrep-json-wrap">
            <pre className="secrep-json">{JSON.stringify(doc, null, 2)}</pre>
          </div>
        ) : (
          <>
            {visible.length === 0 && (
              <div className="secrep-empty">
                {doc.findings.length === 0 ? 'No confirmed findings.' : 'No findings match.'}
              </div>
            )}
            {groups.map(({ sev, items }) => (
              <div key={sev} className="secrep-group">
                <div className="eyebrow">
                  <span className="secrep-dot" style={{background: SEVERITY_COLOR[sev]}}/>
                  {sev} · {items.length}
                </div>
                {items.map(({ f, idx }) => (
                  <FindingCard key={idx} finding={f} sev={severityBucket(f.severity)}
                               open={expanded.has(idx)} onToggle={() => toggle(idx)}/>
                ))}
              </div>
            ))}
          </>
        )}
      </div>
    </div>
  );
}

function FindingCard({ finding: f, sev, open, onToggle }) {
  return (
    <div className="secrep-card">
      <div className="secrep-card-head" onClick={onToggle}>
        <span className="secrep-caret" style={{transform: open ? 'rotate(90deg)' : 'none'}}>
          <Icon name="chev" size={10}/>
        </span>
        <span className="secrep-dot" style={{background: SEVERITY_COLOR[sev]}}/>
        <span className="secrep-card-title">{f.title || f.run_finding_id || f.id}</span>
        {f.key && <span className="chip mono">{f.key}</span>}
        {f.recurring && <span className="chip mono">recurring x{f.occurrences}</span>}
      </div>
      {open && (
        <div className="secrep-card-body">
          {row('class', f.vulnerability_class && <span className="chip mono">{f.vulnerability_class}</span>)}
          {row('boundary', text(f.trust_boundary))}
          {row('flow', (f.entry_point || f.sink_or_decision) && (
            <span className="mono">{f.entry_point}{f.entry_point && f.sink_or_decision ? ' → ' : ''}{f.sink_or_decision}</span>
          ))}
          {row('root cause', text(f.root_cause))}
          {row('reachability', text(f.reachability))}
          {row('impact', text(f.tenant_or_instance_impact))}
          {row('rationale', text(f.severity_rationale))}
          {row('fix', text(f.fix_recommendation))}
          {row('paths', Array.isArray(f.affected_paths) && f.affected_paths.length > 0 && (
            <span className="secrep-paths">
              {f.affected_paths.map((p, i) => <span key={i} className="chip mono">{p}</span>)}
            </span>
          ))}
          {row('evidence', Array.isArray(f.evidence) && f.evidence.length > 0 && (
            <pre className="secrep-pre secrep-pre-wrap">{f.evidence.join('\n')}</pre>
          ))}
          {row('patch', !!(f.suggested_patch && f.suggested_patch.trim()) && (
            <pre className="secrep-pre">{f.suggested_patch}</pre>
          ))}
        </div>
      )}
    </div>
  );
}

function text(v) {
  return v && String(v).trim() ? <span>{v}</span> : null;
}

function row(label, value) {
  if (!value) return null;
  return (
    <React.Fragment key={label}>
      <span className="secrep-label">{label}</span>
      <span className="secrep-value">{value}</span>
    </React.Fragment>
  );
}
