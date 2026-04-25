/* Dyson — right-rail tool panels */

import React, { useState, useEffect, useRef } from 'react';
import { Icon } from './icons.jsx';
import { copyToClipboard } from '../lib/clipboard.js';

function PanelChrome({ icon, name, arg, live, copyText, onClose, children }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = async () => {
    const text = typeof copyText === 'function' ? copyText() : (copyText || '');
    if (await copyToClipboard(text)) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    }
  };
  return (
    <div className={`panel ${live ? 'live' : ''}`}>
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

function ToolPanel({ tool, onClose }) {
  const running = tool.status === 'running';
  const icon = tool.icon || tool.name[0].toUpperCase();
  let body = null;
  switch (tool.kind) {
    case 'bash': body = <BashPanel running={running} body={tool.body}/>; break;
    case 'diff': body = <DiffPanel files={tool.body.files || []}/>; break;
    case 'sbom': body = <SbomPanel rows={tool.body.rows || []} counts={tool.body.counts || {}}/>; break;
    case 'taint': body = <TaintPanel flow={tool.body.flow || []}/>; break;
    case 'read': body = <ReadPanel path={tool.body.path} lines={tool.body.lines || []} highlight={tool.body.highlight}/>; break;
    case 'thinking': body = <ThinkingPanel text={tool.body?.text || ''} running={running}/>; break;
    case 'image': body = <ImagePanel url={tool.body?.url} name={tool.body?.name} prompt={tool.body?.prompt || tool.prompt}/>; break;
    case 'subagent': body = <SubagentPanel children={tool.body?.children} summary={tool.body?.summary} running={running}/>; break;
    default: body = <FallbackPanel text={tool.body?.text || ''}/>;
  }
  return (
    <PanelChrome
      icon={icon}
      name={tool.name}
      arg={tool.sig}
      live={running}
      copyText={() => copyTextForTool(tool)}
      onClose={onClose}
    >
      {body}
    </PanelChrome>
  );
}

export { PanelChrome, BashPanel, DiffPanel, SbomPanel, TaintPanel, ThinkingPanel, ImagePanel, FallbackPanel, ReadPanel, SubagentPanel, ToolPanel, copyTextForTool };
