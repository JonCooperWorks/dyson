/* Dyson — right-rail tool panels */

const { useState: uS1 } = React;

function PanelChrome({ icon, name, arg, live, copyText, onClose, children }) {
  const [copied, setCopied] = uS1(false);
  const handleCopy = async () => {
    const text = typeof copyText === 'function' ? copyText() : (copyText || '');
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed'; ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
      }
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch (_) { /* clipboard denied — swallow */ }
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
  return (
    <div className="p-body flush">
      <table className="sbom">
        <thead><tr><th className="sev">sev</th><th>package</th><th>advisory</th><th>reach</th></tr></thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              <td className="sev"><span className={`b ${r.sev === 'high' ? 'high' : r.sev === 'med' ? 'med' : r.sev === 'low' ? 'low' : 'crit'}`}>{r.sev}</span></td>
              <td><span className="pkg">{r.pkg}</span> <span className="ver">{r.ver}</span><div style={{color:'var(--mute)',fontSize:10.5,marginTop:3}}>{r.note}</div></td>
              <td style={{color:'var(--mute-2)'}}>{r.id}</td>
              <td><span className={`reach ${r.reach==='unreachable'?'no':''}`}>{r.reach}</span></td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="sbom-foot">
        <span>247 crates</span>
        <span style={{color:'var(--err)'}}>1 high</span>
        <span style={{color:'var(--warn)'}}>2 med</span>
        <span>1 low</span>
        <span style={{flex:1}}/>
        <span style={{color:'var(--mute)'}}>2 reachable</span>
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

Object.assign(window, { PanelChrome, BashPanel, DiffPanel, SbomPanel, TaintPanel, FallbackPanel, ReadPanel, ToolPanel, copyTextForTool });
