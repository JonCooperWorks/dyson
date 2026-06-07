/* Dyson — right-rail tool panels */

import React, { useState, useEffect, useRef } from 'react';
import { Icon } from './icons.jsx';
import { copyToClipboard } from '../lib/clipboard.js';

function PanelChrome({ icon, name, arg, live, copyText, onClose, toolRef, children }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = async () => {
    const text = typeof copyText === 'function' ? copyText() : (copyText || '');
    if (await copyToClipboard(text)) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  };
  return (
    <div className={`panel ${live ? 'live' : ''}`} data-tool-ref={toolRef || undefined}>
      <div className="p-head">
        <span className="ic">{icon}</span>
        <span className="t"><span>{name}</span> <span className="fade">· {arg}</span></span>
        <div className="actions">
          <button onClick={handleCopy} className={copied ? 'on' : ''} title={copied ? 'Copied' : 'Copy'}>
            <Icon name={copied ? 'rate' : 'copy'} size={12}/>
          </button>
          <button onClick={onClose} title="Close"><Icon name="x" size={12}/></button>
        </div>
      </div>
      {children}
    </div>
  );
}

function copyTextForTool(tool) {
  const body = tool.body || {};
  switch (tool.kind) {
    case 'bash': {
      const lines = Array.isArray(body.lines) ? body.lines : [];
      return lines.map(l => l.t).join('\n');
    }
    case 'diff': {
      const files = Array.isArray(body.files) ? body.files : [];
      return files.map(f => {
        const head = `${f.path}  +${f.add} -${f.rem}\n${f.hunk || ''}`;
        const rows = (f.rows || []).map(r => {
          const sign = r.t === 'add' ? '+' : r.t === 'rem' ? '-' : ' ';
          return sign + (r.l || '');
        }).join('\n');
        return head + '\n' + rows;
      }).join('\n\n');
    }
    case 'sbom': {
      const rows = Array.isArray(body.rows) ? body.rows : [];
      return rows.map(r => `${r.sev}\t${r.pkg} ${r.ver}\t${r.id}\t${r.reach}\t${r.note || ''}`).join('\n');
    }
    case 'taint': {
      const flow = Array.isArray(body.flow) ? body.flow : [];
      return flow.map(n => `[${n.kind}] ${n.loc} ${n.sym || ''} — ${n.note || ''}`).join('\n');
    }
    case 'read': {
      const lines = Array.isArray(body.lines) ? body.lines : [];
      return (body.path ? `// ${body.path}\n` : '') + lines.join('\n');
    }
    case 'subagent': {
      const children = Array.isArray(body.children) ? body.children : [];
      const list = children.map(c => {
        const status = c.status === 'running' ? 'running' : (c.exit === 'ok' ? 'exit 0' : 'exit 1');
        return `${c.name}\t${c.dur || ''}\t${status}`;
      }).join('\n');
      return body.summary ? `${list}\n\n${body.summary}` : list;
    }
    default:
      return typeof body.text === 'string' ? body.text : '';
  }
}

function copyInputForTool(tool) {
  const body = tool.body || {};
  if (tool.kind === 'bash') {
    const lines = Array.isArray(body.lines) ? body.lines : [];
    return lines
      .filter(l => l.c === 'p' && l.t)
      .map(l => String(l.t).replace(/^\$\s?/, ''))
      .join('\n');
  }
  if (tool.kind === 'read') return body.path || tool.sig || '';
  if (tool.kind === 'image') return body.prompt || tool.prompt || '';
  if (tool.prompt) return tool.prompt;
  return tool.sig || '';
}

function BashPanel({ running, body }) {
  // body shape (from ToolView::Bash): { lines: [{c,t}], exit_code, duration_ms }.
  // Falls back to seed lines so the static prototype still looks plausible.
  const seedLines = [
    { c: 'p', t: '$ cargo test -p dyson-server auth::' },
    { c: 'd', t: '   Compiling dyson-server v0.4.0' },
    { c: 'd', t: '    Finished test [unoptimized + debuginfo] target(s) in 4.82s' },
    { c: 'c', t: 'running 14 tests' },
    { c: 'c', t: 'test auth::tests::extracts_bearer ... ok' },
    { c: 'c', t: 'test auth::tests::decodes_valid_jwt ... ok' },
    { c: 'c', t: 'test auth::tests::rejects_token_without_jti ... ok' },
  ];
  const lines = (body && Array.isArray(body.lines) && body.lines.length > 0) ? body.lines : seedLines;
  const exit = body && typeof body.exit_code === 'number' ? body.exit_code : 0;
  const dur = body && typeof body.duration_ms === 'number'
    ? (body.duration_ms < 1000 ? body.duration_ms + 'ms' : (body.duration_ms / 1000).toFixed(1) + 's')
    : (running ? '…' : '5.8s');
  return (
    <>
      <div className="term p-body flush">
        {lines.map((l, i) => <div key={i} className={`line ${l.c}`}>{l.t}</div>)}
        {running && <div className="line c"><span className="cursor blink"/></div>}
      </div>
      <div className="term-foot">
        <span className="mono">{dur}</span>
        <span className="sep" style={{flex:1}}/>
        <span className={`exit ${running ? '' : (exit === 0 ? 'ok' : 'err')}`}>
          {running ? 'running' : `exit ${exit}`}
        </span>
      </div>
    </>
  );
}

function DiffPanel({ files }) {
  return (
    <div className="p-body flush diff">
      {files.map((f, fi) => (
        <React.Fragment key={fi}>
          <div className="file">
            <span className="path">{f.path}</span>
            <span className="sz"><span className="a">+{f.add}</span><span className="r">−{f.rem}</span></span>
          </div>
          <div className="hunk">{f.hunk}</div>
          {f.rows.map((r, i) => (
            <div key={i} className={`row ${r.t}`}>
              <span className="ln">{r.ln}</span>
              <span className="sn">{r.sn}</span>
              <span className="l">{r.l}</span>
            </div>
          ))}
        </React.Fragment>
      ))}
    </div>
  );
}

function SbomPanel({ rows, counts }) {
  const c = counts || {};
  const total = typeof c.total === 'number' ? c.total : rows.length;
  const crit = c.crit || 0;
  const high = c.high || 0;
  const med = c.med || 0;
  const low = c.low || 0;
  // Reachability isn't computed server-side yet (every row carries
  // `reach: "unknown"` from `build_sbom_view`) — tally whatever did
  // arrive so if a future taint pass fills it in the count shows up.
  const reachable = rows.filter(r => r.reach === 'reachable').length;
  const clean = rows.length === 0;
  return (
    <div className="p-body flush">
      {clean ? (
        <div style={{padding:'20px 16px', color:'var(--mute)', fontSize:12, lineHeight:1.5}}>
          No known vulnerabilities across <strong style={{color:'var(--fg)'}}>{total.toLocaleString()}</strong> {total === 1 ? 'dependency' : 'dependencies'}.
        </div>
      ) : (
        <table className="sbom">
          <thead><tr><th className="sev">sev</th><th>package</th><th>advisory</th><th>reach</th></tr></thead>
          <tbody>
            {rows.map((r, i) => (
              <tr key={i}>
                <td className="sev"><span className={`b ${r.sev === 'high' ? 'high' : r.sev === 'med' ? 'med' : r.sev === 'low' ? 'low' : r.sev === 'crit' ? 'crit' : ''}`}>{r.sev}</span></td>
                <td><span className="pkg">{r.pkg}</span> <span className="ver">{r.ver}</span><div style={{color:'var(--mute)',fontSize:10.5,marginTop:3}}>{r.note}</div></td>
                <td style={{color:'var(--mute-2)'}}>{r.id}</td>
                <td><span className={`reach ${r.reach==='unreachable'?'no':''}`}>{r.reach}</span></td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <div className="sbom-foot">
        <span>{total.toLocaleString()} {total === 1 ? 'crate' : 'crates'}</span>
        {crit > 0 && <span style={{color:'var(--err)'}}>{crit} crit</span>}
        {high > 0 && <span style={{color:'var(--err)'}}>{high} high</span>}
        {med > 0 && <span style={{color:'var(--warn)'}}>{med} med</span>}
        {low > 0 && <span>{low} low</span>}
        {clean && <span style={{color:'var(--ok, var(--fg))'}}>✓ clean</span>}
        <span style={{flex:1}}/>
        {reachable > 0 && <span style={{color:'var(--mute)'}}>{reachable} reachable</span>}
      </div>
    </div>
  );
}

function TaintPanel({ flow }) {
  return (
    <div className="p-body flush taint">
      <div className="flow">
        {flow.map((n, i) => (
          <React.Fragment key={i}>
            <div className={`node ${n.kind}`}>
              <span className="ic">{n.kind === 'source' ? 'S' : n.kind === 'sink' ? '!' : '·'}</span>
              <div className="col">
                <div className="loc">{n.loc} <span style={{color:'var(--mute)',marginLeft:6}}>{n.sym}</span></div>
                <div className="sym">{n.note}</div>
              </div>
            </div>
            {i < flow.length - 1 && <div className="edge"/>}
          </React.Fragment>
        ))}
      </div>
    </div>
  );
}

function FallbackPanel({ text }) {
  return <div className="fallback-body">{text}</div>;
}

// Subagent panel — renders the live list of inner tool calls a
// subagent (security_engineer, coder, etc.) is dispatching, so users
// can watch the inner agent work instead of staring at an empty
// fallback panel for minutes.  The list is fed by `tool_start` /
// `tool_result` events tagged with `parent_tool_id` (see
// `controller::http::SubagentEventBus` on the backend) — no LLM-side
// data flows through here.
//
// `children` shape (one entry per inner tool call):
//   { id, name, status: 'running' | 'done', exit?: 'ok' | 'err',
//     dur?: string, kind?: string, body?: object }
// `summary` is the subagent's final text reply, populated when the
// outer subagent ToolResult arrives.
function SubagentPanel({ children, summary, running }) {
  const list = Array.isArray(children) ? children : [];
  return (
    <div className="p-body flush" style={{overflow:'auto', flex:1}}>
      {list.length === 0 && (
        <div style={{padding:'16px', color:'var(--mute)', fontSize:12}}>
          {running ? 'Subagent starting…' : 'Subagent ran without inner tool calls.'}
        </div>
      )}
      {list.map((c, i) => (
        <div key={c.id || i} className={`toolchip ${c.status === 'running' ? 'running' : ''}`}
             style={{margin:'6px 10px', cursor:'default'}}>
          <span className="icon">{(c.name && c.name[0] || '?').toUpperCase()}</span>
          <span className="sig"><span className="tname">{c.name}</span></span>
          <span className="meta">
            <span className="dur">{c.dur || (c.status === 'running' ? '…' : '')}</span>
            {c.status === 'done' && (
              <span className={`exit ${c.exit === 'ok' ? 'ok' : 'err'}`}>
                {c.exit === 'ok' ? 'exit 0' : 'exit 1'}
              </span>
            )}
            {c.status === 'running' && <span className="exit">…</span>}
          </span>
        </div>
      ))}
      {summary && (
        <div style={{padding:'12px 14px', borderTop:'1px solid var(--line)',
                     fontSize:12, lineHeight:1.55, color:'var(--fg-dim)',
                     whiteSpace:'pre-wrap'}}>
          {summary}
        </div>
      )}
    </div>
  );
}

// -------------------------------------------------------------------------
// Security harness — first-class panel for the `security_engineer` tool.
//
// The harness runs a fixed 8-stage pipeline (Recon → Hunt → Validate →
// Gapfill → Dedupe → Trace → Feedback → Report).  Without this panel the
// operator stares at a long list of read_file/ast_query chips with no
// signal about which stage is active, how many findings have accumulated,
// or whether validate is about to bite.  The backend already emits a
// `checkpoint` event with `message: "security_engineer: <stage>"` between
// stages (see `security_engineer/mod.rs` and `stages.rs`), and those text
// lines get appended to the live tool's body via `onCheckpoint`.  We
// parse them here to derive the stage state — zero backend changes for
// this MVP cut.  A future event-payload upgrade can replace the parsing
// with structured fields.

const HARNESS_STAGES = [
  'recon', 'hunt', 'validate', 'gapfill', 'dedupe', 'trace', 'feedback', 'report',
];

const STAGE_LABEL = {
  recon: 'Recon',
  hunt: 'Hunt',
  validate: 'Validate',
  gapfill: 'Gapfill',
  dedupe: 'Dedupe',
  trace: 'Trace',
  feedback: 'Feedback',
  report: 'Report',
};

const SEVERITY_LABELS = ['critical', 'high', 'medium', 'low'];
const SEVERITY_COLOR = {
  critical: '#cf2a2a',
  high: '#d97706',
  medium: '#ca8a04',
  low: '#6b7280',
};

// Parse the running text of `security_engineer` checkpoints into a state
// record.  The text accumulates messages like
//   security_engineer: starting checkpoint sec-...
//   security_engineer: recon
//   security_engineer: hunt
//   security_engineer: hunt: class auth_authorization hunted (3 findings)
//   security_engineer: hunt: class session_oauth_csrf cleared
//   security_engineer: findings critical=1 high=20 medium=48 low=47
//   security_engineer: validate
//   security_engineer: validate failed: no JSON object found in stage output
//   security_engineer: completed sec-... in 4521s
//
// Every UI signal the panel surfaces is derived from this stream — zero
// backend changes required beyond emitting the right strings.
function parseHarnessState(text, isRunning, exitErr = false) {
  const lines = (text || '').split('\n').map(l => l.trim()).filter(Boolean);
  let runId = null;
  let lastStage = null;
  let completed = false;
  let resumed = false;
  let failedAtStage = null;
  let failureMessage = null;
  const findings = { critical: 0, high: 0, medium: 0, low: 0 };
  const classStatus = {}; // class_id -> {status, count}

  for (const line of lines) {
    const idMatch = line.match(/sec-[0-9a-z-]+/);
    if (idMatch && !runId) runId = idMatch[0];
    if (/\bresume\b/i.test(line)) resumed = true;
    if (/\bcompleted\b/i.test(line)) { completed = true; continue; }

    // class hunt outcome: `hunt: class <id> hunted (N findings)` / `cleared` / `inapplicable`
    const classMatch = line.match(/hunt:\s*class\s+([a-z_]+)\s+(hunted|cleared|inapplicable)(?:\s*\((\d+)\s+findings?\))?/i);
    if (classMatch) {
      const [, cls, status, countStr] = classMatch;
      classStatus[cls] = { status: status.toLowerCase(), count: countStr ? parseInt(countStr, 10) : 0 };
      continue;
    }

    // findings counter line: `findings critical=N high=N medium=N low=N`
    const findingsMatch = line.match(/findings\s+critical=(\d+)\s+high=(\d+)\s+medium=(\d+)\s+low=(\d+)/i);
    if (findingsMatch) {
      findings.critical = parseInt(findingsMatch[1], 10);
      findings.high = parseInt(findingsMatch[2], 10);
      findings.medium = parseInt(findingsMatch[3], 10);
      findings.low = parseInt(findingsMatch[4], 10);
      continue;
    }

    // stage failure: `<stage> failed: <message>` or `<stage> error: <message>`
    const failMatch = line.match(/(recon|hunt|validate|gapfill|dedupe|trace|feedback|report)\s+(failed|error):\s*(.+)/i);
    if (failMatch) {
      failedAtStage = failMatch[1].toLowerCase();
      failureMessage = failMatch[3].trim();
      continue;
    }

    // bare error line — captured for the panel error banner even without
    // a stage label, so an early-aborted run still surfaces SOMETHING.
    const bareErrMatch = line.match(/^security_engineer:\s+error\b\s*(.*)/i);
    if (bareErrMatch && !failureMessage) {
      failureMessage = bareErrMatch[1].trim() || line;
      continue;
    }

    // `security_engineer: <stage>` exactly
    const sm = line.match(/security_engineer:\s*([a-z]+)\b/i);
    if (sm) {
      const s = sm[1].toLowerCase();
      if (HARNESS_STAGES.includes(s)) lastStage = s;
    }
  }

  const currentIdx = lastStage ? HARNESS_STAGES.indexOf(lastStage) : -1;
  const errored = exitErr || !!failedAtStage;
  // For a failed run, the failed stage renders as "errored", not "done".
  // For a pending-but-running stage that errored unexpectedly, the same.
  const stageStatus = HARNESS_STAGES.map((s, i) => {
    if (completed) return 'done';
    if (errored && failedAtStage === s) return 'errored';
    if (errored && lastStage === s && !failedAtStage) return 'errored';
    if (i < currentIdx) return 'done';
    if (i === currentIdx) {
      if (errored) return 'errored';
      return isRunning ? 'running' : 'done';
    }
    return 'pending';
  });

  const totalFindings = findings.critical + findings.high + findings.medium + findings.low;
  return {
    runId, lastStage, completed, resumed,
    failedAtStage, failureMessage, errored,
    stageStatus, findings, totalFindings, classStatus,
  };
}

// Stage progress bar — 8 cells with status-coded background.  Larger
// labels than the MVP (12px vs 10px) since live evaluation showed the
// old size was squinty against deepseek-v4-pro's real outputs.  Errored
// cells get a distinct red background + strikethrough so a validate
// fail is unmistakable next to a clean recon→hunt completion.
function StageBar({ status }) {
  return (
    <div style={{display:'flex', gap:4, padding:'10px 12px',
                 borderBottom:'1px solid var(--line)',
                 background:'var(--bg-1)'}}>
      {HARNESS_STAGES.map((s, i) => {
        const st = status[i];
        const bg = st === 'done' ? 'var(--ok, #2c7a3a)'
                 : st === 'running' ? 'var(--accent)'
                 : st === 'errored' ? 'var(--err, #b91c1c)'
                 : 'var(--bg)';
        const fg = st === 'pending' ? 'var(--mute)' : 'var(--fg)';
        const border = st === 'pending'
          ? '1px solid var(--line)'
          : '1px solid transparent';
        const decoration = st === 'errored' ? 'line-through' : 'none';
        return (
          <div key={s} title={STAGE_LABEL[s]} style={{
            flex:1, fontSize:12, lineHeight:'22px', textAlign:'center',
            background: bg, color: fg, border, borderRadius:4,
            fontWeight: st === 'running' || st === 'errored' ? 600 : 400,
            letterSpacing: 0.3,
            textDecoration: decoration,
          }}>
            {STAGE_LABEL[s]}{st === 'running' && ' …'}
          </div>
        );
      })}
    </div>
  );
}

// Live findings counter — color-coded counts by severity. Hides when
// there are zero findings (typical for runs that died before hunt).
function FindingsCounter({ findings, total }) {
  if (!total) return null;
  return (
    <div style={{display:'flex', gap:14, padding:'8px 12px',
                 borderBottom:'1px solid var(--line)',
                 background:'var(--bg)',
                 fontSize:12, color:'var(--fg-dim)',
                 alignItems:'baseline'}}>
      <span style={{fontWeight:600, color:'var(--fg)'}}>
        {total} {total === 1 ? 'finding' : 'findings'}
      </span>
      {SEVERITY_LABELS.map(sev => {
        const n = findings[sev] || 0;
        if (!n) return null;
        return (
          <span key={sev} style={{display:'inline-flex', alignItems:'center', gap:4}}>
            <span style={{
              display:'inline-block', width:7, height:7, borderRadius:'50%',
              background: SEVERITY_COLOR[sev],
            }}/>
            <span style={{color:'var(--fg)'}}>{n}</span>
            <span style={{fontVariant:'small-caps', letterSpacing:0.5}}>{sev}</span>
          </span>
        );
      })}
    </div>
  );
}

// Per-class hunt status grid — one cell per taxonomy class, colored
// by whether the class was hunted (and with findings), cleared
// (hunted, none found), inapplicable (skipped via stack pruning), or
// still pending.  Cells are clickable-shaped but click handling is a
// Phase 3 follow-up (drill-into-findings-for-this-class).
function ClassGrid({ classStatus }) {
  const entries = Object.entries(classStatus || {});
  if (entries.length === 0) return null;
  // Sort by status priority (cells with findings first → easier eye scan)
  const priority = { hunted: 0, cleared: 1, inapplicable: 2 };
  entries.sort(([, a], [, b]) =>
    (priority[a.status] ?? 9) - (priority[b.status] ?? 9));
  return (
    <div style={{padding:'8px 12px', borderBottom:'1px solid var(--line)',
                 background:'var(--bg)'}}>
      <div style={{fontSize:10, letterSpacing:0.5, color:'var(--mute)',
                   textTransform:'uppercase', marginBottom:6}}>
        Class coverage ({entries.length}/24 reported)
      </div>
      <div style={{display:'grid', gridTemplateColumns:'repeat(auto-fill, minmax(140px, 1fr))', gap:4}}>
        {entries.map(([cls, info]) => {
          const bg = info.status === 'hunted' ? 'var(--ok-dim, #1f4a2a)'
                   : info.status === 'cleared' ? 'var(--bg-1)'
                   : info.status === 'inapplicable' ? 'transparent'
                   : 'var(--bg-1)';
          const opacity = info.status === 'inapplicable' ? 0.45 : 1;
          return (
            <div key={cls} title={`${cls} — ${info.status}${info.count ? ` (${info.count})` : ''}`}
                 style={{
                   padding:'4px 8px', borderRadius:3, fontSize:10,
                   background: bg,
                   color:'var(--fg-dim)',
                   border:'1px solid var(--line)',
                   opacity,
                   display:'flex', justifyContent:'space-between', alignItems:'center',
                   fontFamily:'var(--font-mono)',
                 }}>
              <span style={{overflow:'hidden', textOverflow:'ellipsis', whiteSpace:'nowrap'}}>
                {cls}
              </span>
              {info.count > 0 && (
                <span style={{color:'var(--fg)', fontWeight:600, marginLeft:6}}>
                  {info.count}
                </span>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

// Header — run_id, resume / completed / failed badges, error banner.
function HarnessHeader({ runId, resumed, completed, errored, errorText, summary, failedAtStage }) {
  return (
    <div style={{padding:'10px 12px', borderBottom:'1px solid var(--line)',
                 fontSize:11, color:'var(--fg-dim)', background:'var(--bg)'}}>
      <div style={{display:'flex', alignItems:'center', gap:10, flexWrap:'wrap'}}>
        <span style={{fontFamily:'var(--font-mono)', color:'var(--fg)', fontSize:12}}>
          {runId || '(no run id yet)'}
        </span>
        {resumed && (
          <span style={{fontSize:10, padding:'2px 7px', borderRadius:3,
                        background:'var(--bg-1)', border:'1px solid var(--line)',
                        color:'var(--fg-dim)', letterSpacing:0.3}}>
            resumed
          </span>
        )}
        {completed && (
          <span style={{fontSize:10, padding:'2px 7px', borderRadius:3,
                        background:'var(--ok, #2c7a3a)', color:'var(--fg)',
                        fontWeight:600, letterSpacing:0.3}}>
            completed
          </span>
        )}
        {errored && (
          <span style={{fontSize:10, padding:'2px 7px', borderRadius:3,
                        background:'var(--err, #b91c1c)', color:'var(--fg)',
                        fontWeight:600, letterSpacing:0.3}}>
            {failedAtStage ? `failed at ${STAGE_LABEL[failedAtStage]}` : 'failed'}
          </span>
        )}
      </div>
      {errorText && (
        <div style={{marginTop:8, padding:'8px 10px', borderRadius:4,
                     background:'var(--err-dim, #4a1f1f)', color:'var(--fg)',
                     fontSize:11, lineHeight:1.5, whiteSpace:'pre-wrap',
                     borderLeft:'3px solid var(--err, #b91c1c)'}}>
          {errorText}
        </div>
      )}
      {summary && !errorText && (
        <div style={{marginTop:6, fontSize:11, color:'var(--fg-dim)',
                     whiteSpace:'pre-wrap'}}>
          {summary}
        </div>
      )}
    </div>
  );
}

// Top-level harness panel.  Stack order:
//   [stage bar]
//   [run header w/ run_id, resumed/completed/failed badges, error banner]
//   [findings counter]   (hidden when 0 findings)
//   [class coverage grid] (hidden when no class events seen)
//   [inner tool list — existing SubagentPanel]
function SecurityHarnessPanel({ body, exit, running, summary, errorText }) {
  const text = body?.text || '';
  const exitErr = exit === 'err';
  const state = parseHarnessState(text, running, exitErr);
  // Derive the error banner from the parsed failureMessage when an
  // explicit errorText wasn't supplied (the typical case for live runs:
  // tool.exit === 'err' but the caller doesn't pre-extract a message).
  const effectiveErrorText = errorText || state.failureMessage
    || (exitErr ? 'Harness returned an error — no message captured.' : null);

  return (
    <div className="p-body flush" style={{overflow:'auto', flex:1,
                                          display:'flex', flexDirection:'column'}}>
      <StageBar status={state.stageStatus}/>
      <HarnessHeader
        runId={state.runId}
        resumed={state.resumed}
        completed={state.completed}
        errored={state.errored}
        failedAtStage={state.failedAtStage}
        errorText={effectiveErrorText}
        summary={state.completed ? summary : null}
      />
      <FindingsCounter findings={state.findings} total={state.totalFindings}/>
      <ClassGrid classStatus={state.classStatus}/>
      <div style={{flex:1, minHeight:0, display:'flex', flexDirection:'column'}}>
        <SubagentPanel
          children={body?.children}
          summary={state.completed ? null : summary}
          running={running}
        />
      </div>
    </div>
  );
}

function ReadPanel({ path, lines, highlight }) {
  return (
    <div className="p-body flush" style={{background:'var(--bg)'}}>
      <div style={{padding:'8px 12px', borderBottom:'1px solid var(--line)', fontFamily:'var(--font-mono)', fontSize:11, color:'var(--mute)'}}>{path}</div>
      <div style={{fontFamily:'var(--font-mono)', fontSize:11.5, lineHeight:1.6, padding:'8px 0'}}>
        {lines.map((l, i) => (
          <div key={i} style={{display:'flex', background: i+1===highlight ? 'var(--accent-dim)' : 'transparent', padding:'0 12px 0 0'}}>
            <span style={{color:'var(--mute)', width:32, textAlign:'right', paddingRight:10, flex:'0 0 auto', userSelect:'none'}}>{i+1}</span>
            <span style={{color: i+1===highlight ? 'var(--fg)' : 'var(--fg-dim)', whiteSpace:'pre'}}>{l || ' '}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

// Live reasoning panel.  Streams extended-thinking deltas from the
// model into a scroll-locked mono-space view so the user can watch it
// reason before the text starts.  Auto-scrolls to the bottom on each
// update, matching BashPanel's UX.
function ThinkingPanel({ text, running }) {
  const bodyRef = useRef(null);
  useEffect(() => {
    if (bodyRef.current) bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
  }, [text]);
  if (!text && !running) {
    return <div style={{padding:16, color:'var(--mute)', fontSize:12}}>No reasoning yet.</div>;
  }
  return (
    <div ref={bodyRef}
         style={{overflowY:'auto', flex:1, padding:'12px 14px',
                 fontFamily:'ui-monospace, SFMono-Regular, Menlo, monospace',
                 fontSize:12, lineHeight:1.55, whiteSpace:'pre-wrap',
                 color:'var(--fg-dim)', background:'var(--bg)'}}>
      {text || ''}
      {running && <span style={{color:'var(--accent)', marginLeft:4}}>▍</span>}
    </div>
  );
}

// Image-result panel — surfaces the generated image in the right-rail
// tool stack so it's visible alongside the tool call that produced it.
function ImagePanel({ url, name, prompt }) {
  if (!url) {
    return <div style={{padding:16, color:'var(--mute)', fontSize:12}}>No image URL.</div>;
  }
  return (
    <div style={{overflow:'auto', flex:1, padding:14, display:'flex',
                 flexDirection:'column', alignItems:'center', gap:10,
                 background:'var(--bg)'}}>
      <a href={url} target="_blank" rel="noopener" title={name}
         style={{display:'block', maxWidth:'100%'}}>
        <img src={url} alt={name || 'generated image'}
             style={{maxWidth:'100%', maxHeight:'100%', objectFit:'contain',
                     borderRadius:4, boxShadow:'0 2px 10px rgba(0,0,0,0.15)'}}/>
      </a>
      {prompt && (
        <div style={{alignSelf:'stretch', fontSize:12, lineHeight:1.4,
                     color:'var(--fg-dim)', fontStyle:'italic'}}>
          {prompt}
        </div>
      )}
    </div>
  );
}

// Renders just the kind-specific body of a tool, without the panel chrome
// header.  Used inline in the chat transcript where the tool chip itself
// already serves as the header — stacking PanelChrome on top would
// duplicate the icon, name, and arg row.
function ToolBody({ tool }) {
  const running = tool.status === 'running';
  // The security_engineer tool deserves a first-class panel — operators
  // need to see stage progression, findings count, and resume state at
  // a glance.  Route by tool.name before falling through to kind so the
  // backend doesn't have to invent a new kind just to opt in.
  if (tool.name === 'security_engineer') {
    // Pass tool.exit through so the panel can auto-derive an error
    // banner from body.text when the run failed.  Don't pre-extract
    // errorText — the panel's parser does it better (last failure
    // line in the checkpoint stream, with stage attribution).
    return <SecurityHarnessPanel
             body={tool.body}
             exit={tool.exit}
             running={running}
             summary={tool.body?.summary}/>;
  }
  switch (tool.kind) {
    case 'bash':     return <BashPanel running={running} body={tool.body}/>;
    case 'diff':     return <DiffPanel files={tool.body?.files || []}/>;
    case 'sbom':     return <SbomPanel rows={tool.body?.rows || []} counts={tool.body?.counts || {}}/>;
    case 'taint':    return <TaintPanel flow={tool.body?.flow || []}/>;
    case 'read':     return <ReadPanel path={tool.body?.path} lines={tool.body?.lines || []} highlight={tool.body?.highlight}/>;
    case 'thinking': return <ThinkingPanel text={tool.body?.text || ''} running={running}/>;
    case 'image':    return <ImagePanel url={tool.body?.url} name={tool.body?.name} prompt={tool.body?.prompt || tool.prompt}/>;
    case 'subagent': return <SubagentPanel children={tool.body?.children} summary={tool.body?.summary} running={running}/>;
    default:         return <FallbackPanel text={tool.body?.text || ''}/>;
  }
}

function ToolPanel({ tool, onClose, toolRef }) {
  const running = tool.status === 'running';
  const icon = tool.icon || tool.name[0].toUpperCase();
  return (
    <PanelChrome
      icon={icon}
      name={tool.name}
      arg={tool.sig}
      live={running}
      toolRef={toolRef}
      copyText={() => copyTextForTool(tool)}
      onClose={onClose}
    >
      <ToolBody tool={tool}/>
    </PanelChrome>
  );
}

export { PanelChrome, BashPanel, DiffPanel, SbomPanel, TaintPanel, ThinkingPanel, ImagePanel, FallbackPanel, ReadPanel, SubagentPanel, SecurityHarnessPanel, FindingsCounter, ClassGrid, parseHarnessState, HARNESS_STAGES, SEVERITY_LABELS, ToolBody, ToolPanel, copyTextForTool, copyInputForTool };
