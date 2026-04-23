/* Dyson — turns, subagents, composer */

const { useState: uS2, useRef: uR2, useEffect: uE2 } = React;

function ThinkingBlock({ text }) {
  return (
    <details className="thinking">
      <summary><span className="caret"><Icon name="chev" size={10}/></span> thinking</summary>
      <div className="body">{text}</div>
    </details>
  );
}

function ToolChip({ tool, onOpen, active }) {
  const running = tool.status === 'running';
  return (
    <div className={`toolchip ${active ? 'active' : ''} ${running ? 'running' : ''}`} onClick={onOpen}>
      <span className="icon">{tool.icon}</span>
      <span className="sig"><span className="tname">{tool.name}</span>{tool.sig}</span>
      <span className="meta">
        <span className="dur">{tool.dur}</span>
        <span className={`exit ${tool.exit === 'ok' ? 'ok' : 'err'}`}>
          {running ? '…' : tool.exit === 'ok' ? 'exit 0' : 'exit 1'}
        </span>
      </span>
      <span className="open">open <Icon name="chev" size={10}/></span>
    </div>
  );
}

// Telegram-equivalent emoji rating set.  Order = visual order in the
// floater bar; the row reads negative→neutral→positive left-to-right.
const REACTIONS = ['💩', '👎', '😐', '👍', '🔥', '❤️'];

function Reactions({ turnIndex, current, onPick }) {
  // Renders the floating emoji bar.  When `current` is set the chosen
  // emoji is also rendered as a persistent badge attached to the turn,
  // so the user can see their rating without hovering.  Click again to
  // clear; pick a different one to swap.
  if (turnIndex == null) return null;
  return (
    <div className="reactions">
      {REACTIONS.map(e => (
        <button
          key={e}
          className={`reaction ${current === e ? 'on' : ''}`}
          onClick={(ev) => { ev.stopPropagation(); onPick(current === e ? '' : e); }}
          title={current === e ? 'remove rating' : 'rate'}>
          {e}
        </button>
      ))}
    </div>
  );
}

// Plain-text serialisation of a turn for the copy button.  Includes
// text and thinking blocks (the actual prose); skips tool chips, files,
// and artefact chips since those are UI affordances, not content.
function turnToText(turn) {
  const parts = [];
  for (const b of (turn.blocks || [])) {
    if (b.type === 'text' && b.text) parts.push(b.text);
    else if (b.type === 'thinking' && b.text) parts.push(b.text);
  }
  return parts.join('\n\n');
}

async function copyText(text) {
  if (!text) return false;
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
    return true;
  } catch (_) { return false; }
}

function Turn({ turn, tools, onOpenTool, activeTool, turnIndex, rating, onRate,
                reactionsOpen, onToggleReactions }) {
  const isUser = turn.role === 'user';
  const avatarL = isUser ? 'JC' : 'DY';
  const ratable = !isUser && turnIndex != null && typeof onRate === 'function';
  const [copied, setCopied] = uS2(false);

  // Tap-to-reveal the reactions bar on touch.  Desktop hover path is
  // preserved by CSS gated on @media (hover: hover) and (pointer: fine).
  // Skip interactive descendants so they keep their own semantics, and
  // skip when a text selection exists so copy-from-prose still works.
  const onTurnPointerUp = (e) => {
    if (!ratable || typeof onToggleReactions !== 'function') return;
    if (e.target.closest('button, a, summary, .toolchip, .reactions, .fileblock')) return;
    if (window.getSelection && window.getSelection().toString().length > 0) return;
    onToggleReactions();
  };

  const onCopy = async (e) => {
    e.stopPropagation();
    const ok = await copyText(turnToText(turn));
    if (!ok) return;
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const cls = `turn ${ratable ? 'ratable' : ''} ${reactionsOpen ? 'reactions-open' : ''}`.trim();
  return (
    <div className={cls} onPointerUp={onTurnPointerUp}>
      <div className={`avatar ${isUser ? 'user' : 'agent'}`}>{avatarL}</div>
      <div className="col">
        <div className="who">
          <span className="name">{isUser ? 'jcooper' : 'dyson'}</span>
          {turn.model && <span className="model">{turn.model}</span>}
          <span className="when">{turn.ts}</span>
          {rating && <span className="rating-badge" title={`rated ${rating}`}>{rating}</span>}
          <button
            className={`copy-turn ${copied ? 'on' : ''}`}
            onClick={onCopy}
            title={copied ? 'Copied' : 'Copy message'}>
            <Icon name={copied ? 'rate' : 'copy'} size={11}/>
          </button>
        </div>
        {turn.blocks.map((b, i) => {
          if (b.type === 'text') {
            return <div key={i} className="prose" dangerouslySetInnerHTML={{__html: markdown(b.text)}}/>;
          }
          if (b.type === 'thinking') {
            return <ThinkingBlock key={i} text={b.text}/>;
          }
          if (b.type === 'tool') {
            const t = tools[b.ref];
            if (!t) return null;
            return <ToolChip key={i} tool={t} onOpen={() => onOpenTool(b.ref)} active={activeTool === b.ref}/>;
          }
          if (b.type === 'file') {
            return <FileBlock key={i} block={b}/>;
          }
          if (b.type === 'artefact') {
            return <ArtefactBlock key={i} block={b}/>;
          }
          return null;
        })}
        {ratable && <Reactions turnIndex={turnIndex} current={rating} onPick={(e) => onRate(turnIndex, e)}/>}
      </div>
    </div>
  );
}

// Markdown renderer — handles headers, fenced code, inline code, bold,
// italics, links, blockquotes, ordered + unordered lists, paragraphs.
// No deps; built for chat turns where the input is short and the output
// is HTML safely escaped on the way in.
function markdown(s) {
  if (!s) return '';
  // Strip the broken `![alt](sandbox:///…)` / `![alt](attachment:…)`
  // markdown the model hallucinates alongside a real image_generate
  // tool call — the "URL" isn't fetchable and the raw syntax leaks
  // into the chat.  Image delivery always comes through the tool's
  // file event, not inline markdown.
  s = s.replace(/!\[[^\]]*\]\((?:sandbox:|attachment:)[^)]*\)/gi, '');
  const esc = (t) => t.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');

  // Placeholder bytes for fenced code + inline code MUST be characters
  // the browser doesn't strip and that don't trip any markdown regex
  // below.  The original code used \u0000 / \u0001 — those are control
  // chars that DOMs strip on innerHTML assignment, so the LITERAL
  // `CODE0` / `CODEBLOCK0` text was leaking into chat output (see
  // controller/http/mod.rs `markdown_inline_code_does_not_leak`
  // regression).  `§§` is printable, very rare in normal text, and
  // contains no markdown syntax characters (`*`, `_`, `[`, `(`, `` ` ``).
  const FBEG = '\u00A7\u00A7F';
  const FEND = '\u00A7\u00A7';
  const CBEG = '\u00A7\u00A7C';
  const CEND = '\u00A7\u00A7';

  // Pull fenced code blocks out first so their contents are NOT touched
  // by the inline rules below.  Restored at the end.
  const codeBlocks = [];
  s = s.replace(/```([a-zA-Z0-9_-]*)\n([\s\S]*?)```/g, (_, lang, body) => {
    const i = codeBlocks.length;
    codeBlocks.push({ lang, body });
    return `${FBEG}${i}${FEND}`;
  });

  // Headers that aren't separated from the following content by a
  // blank line (`## CRITICAL\nNo findings.`) fell through to the
  // paragraph branch and rendered as literal `## CRITICAL` text.
  // Inject an extra newline after every header line so block splitting
  // on `\n{2,}` peels the header into its own block.
  s = s.replace(/^(#{1,6}\s+.+)$/gm, '$1\n');

  // Block-level pass: split on blank lines, classify each block.
  const fenceRe = new RegExp(`^${FBEG}(\\d+)${FEND}$`);
  const out = s.split(/\n{2,}/).map(block => {
    const trimmed = block.trim();
    if (!trimmed) return '';
    // Code block placeholder?
    const m = trimmed.match(fenceRe);
    if (m) {
      const cb = codeBlocks[Number(m[1])];
      const langClass = cb.lang ? ` class="lang-${esc(cb.lang)}"` : '';
      return `<pre><code${langClass}>${esc(cb.body.replace(/\n$/, ''))}</code></pre>`;
    }
    // Headers
    const h = trimmed.match(/^(#{1,6})\s+(.+)$/);
    if (h) {
      const level = h[1].length;
      return `<h${level}>${inline(h[2])}</h${level}>`;
    }
    // Blockquote
    if (trimmed.split('\n').every(l => l.startsWith('>'))) {
      const inner = trimmed.split('\n').map(l => l.replace(/^>\s?/, '')).join('<br/>');
      return `<blockquote>${inline(inner)}</blockquote>`;
    }
    // GFM table: first row is header, second is separator |---|, rest are rows.
    const tableLines = trimmed.split('\n');
    if (tableLines.length >= 2 && isTableRow(tableLines[0]) && isTableSeparator(tableLines[1])) {
      const aligns = parseTableAligns(tableLines[1]);
      const head = parseTableRow(tableLines[0]);
      const body = tableLines.slice(2).filter(isTableRow).map(parseTableRow);
      const cell = (tag, txt, i) => {
        const a = aligns[i] || '';
        const style = a ? ` style="text-align:${a}"` : '';
        return `<${tag}${style}>${inline(txt)}</${tag}>`;
      };
      const headHtml = `<thead><tr>${head.map((c, i) => cell('th', c, i)).join('')}</tr></thead>`;
      const bodyHtml = body.length
        ? `<tbody>${body.map(row => `<tr>${row.map((c, i) => cell('td', c, i)).join('')}</tr>`).join('')}</tbody>`
        : '';
      return `<table>${headHtml}${bodyHtml}</table>`;
    }
    // Unordered list
    if (trimmed.split('\n').every(l => /^\s*[-*]\s+/.test(l))) {
      const items = trimmed.split('\n').map(l => `<li>${inline(l.replace(/^\s*[-*]\s+/, ''))}</li>`);
      return `<ul>${items.join('')}</ul>`;
    }
    // Ordered list
    if (trimmed.split('\n').every(l => /^\s*\d+\.\s+/.test(l))) {
      const items = trimmed.split('\n').map(l => `<li>${inline(l.replace(/^\s*\d+\.\s+/, ''))}</li>`);
      return `<ol>${items.join('')}</ol>`;
    }
    // Paragraph — inline format, single newlines become <br/>.
    return `<p>${inline(trimmed).replace(/\n/g, '<br/>')}</p>`;
  }).join('');

  return out;

  function isTableRow(l) {
    const t = l.trim();
    return t.length > 0 && t.includes('|') && (t.startsWith('|') || /[^\\]\|/.test(t));
  }
  function isTableSeparator(l) {
    return l.trim().split('|').filter(Boolean).every(seg => /^\s*:?-{2,}:?\s*$/.test(seg));
  }
  function parseTableRow(l) {
    let s = l.trim();
    if (s.startsWith('|')) s = s.slice(1);
    if (s.endsWith('|')) s = s.slice(0, -1);
    return s.split('|').map(c => c.trim());
  }
  function parseTableAligns(sep) {
    return parseTableRow(sep).map(seg => {
      const left = seg.startsWith(':');
      const right = seg.endsWith(':');
      if (left && right) return 'center';
      if (right) return 'right';
      if (left) return 'left';
      return '';
    });
  }

  function inline(t) {
    // Split on backtick-bounded code spans first so their contents are
    // never touched by bold/italic/link passes.  Even-indexed parts =
    // outside code, odd-indexed = inside `…`.  Avoids the placeholder
    // dance the original used (which got eaten by the DOM stripping
    // control chars on innerHTML assignment).
    const parts = t.split(/(`[^`]+`)/g);
    return parts.map((p, i) => {
      if (i % 2 === 1) {
        return `<code>${esc(p.slice(1, -1))}</code>`;
      }
      let h = esc(p);
      // Links: [text](url) — only http(s) URLs to keep it safe.
      h = h.replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
      // Bold: **x** or __x__
      h = h.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
      h = h.replace(/__([^_]+)__/g, '<strong>$1</strong>');
      // Italic: *x* or _x_  (avoid eating bold contents)
      h = h.replace(/(^|[^*])\*([^*\n]+)\*([^*]|$)/g, '$1<em>$2</em>$3');
      h = h.replace(/(^|[^_])_([^_\n]+)_([^_]|$)/g, '$1<em>$2</em>$3');
      return h;
    }).join('');
  }
}

// Renders an attachment, agent-produced file, or local upload preview.
// Image MIME → inline <img>; everything else → download link with name
// + size.  `block` shape:
//   { type:'file', name, mime, size?, url, inline?, local? }
function FileBlock({ block }) {
  const isImage = (block.mime || '').startsWith('image/') || block.inline;
  if (isImage && block.url) {
    return (
      <a href={block.url} target="_blank" rel="noopener" className="fileblock image">
        <img src={block.url} alt={block.name || 'image'}/>
        <span className="cap">{block.name}{block.local ? ' · uploaded' : ''}</span>
      </a>
    );
  }
  // Non-image: card with name, size, download link.  Local uploads
  // (block.local && !block.url) have no fetchable URL — just show the
  // metadata.
  return (
    <div className="fileblock">
      <Icon name="file" size={14}/>
      <span className="name">{block.name || 'file'}</span>
      {typeof block.size === 'number' && <span className="sz mono">{prettySize(block.size)}</span>}
      {block.url && <a href={block.url} target="_blank" rel="noopener" className="dl mono">download</a>}
    </div>
  );
}

// Rendered in the chat scroll when the agent emits an artefact.
// For image artefacts we render the actual image inline (the file URL
// is reached by asking `/api/artefacts/<id>`, which hands back the
// `/api/files/<id>` URL as plain text).  For markdown / report
// artefacts, a compact chip that opens the Artefacts tab on click.
//
// `block.url` is an SPA deep-link (`/#/artefacts/<id>`) — set so that
// cmd-click / copy-paste of the chip lands on the reader instead of
// the raw bytes endpoint.
//
// `block` shape: { type:'artefact', id, kind, title, url, bytes }
function ArtefactBlock({ block }) {
  const open = (e) => {
    e.preventDefault();
    window.dispatchEvent(new CustomEvent('dyson:open-artefact', { detail: { id: block.id } }));
  };
  // Deep-link to the reader, used as the href fallback so cmd-click
  // always has somewhere sensible to go — never `#` (which would jump
  // the page to the top).
  const reader = block.url || (block.id ? `/#/artefacts/${encodeURIComponent(block.id)}` : '#');

  if (block.kind === 'image') {
    // For a zero-hop preview we fetch the file URL once on mount and
    // swap to <img>.  Until the fetch lands, cmd-click opens the
    // reader (via `reader`); once resolved, cmd-click opens the image
    // itself in a new tab — which is what users reach for on an image.
    const [src, setSrc] = React.useState('');
    React.useEffect(() => {
      if (!block.id) return;
      fetch(`/api/artefacts/${encodeURIComponent(block.id)}`)
        .then(r => r.ok ? r.text() : Promise.reject(r.status))
        .then(text => setSrc(text.trim()))
        .catch(() => {});
    }, [block.id]);
    return (
      <a href={src || reader} target="_blank" rel="noopener" onClick={open}
         className="fileblock image" title={block.title || 'image'}>
        {src
          ? <img src={src} alt={block.title || 'image'}/>
          : <div style={{width:220, height:160, background:'var(--panel)', borderRadius:4}}/>}
        <span className="cap">{block.title || 'image'}</span>
      </a>
    );
  }

  const kind = (block.kind || 'other').replace(/_/g, ' ');
  return (
    <a href={reader} onClick={open} className="fileblock" title="Open artefact">
      <Icon name="file" size={14}/>
      <span className="name">{block.title || 'Artefact'}</span>
      <span className="sz mono" style={{color:'var(--fg-dim)'}}>{kind}</span>
      {typeof block.bytes === 'number' && (
        <span className="sz mono">{prettySize(block.bytes)}</span>
      )}
      <span className="dl mono">open →</span>
    </a>
  );
}

function TypingIndicator({ phase, tname, onJump }) {
  if (phase === 'thinking') {
    return <div className="typing"><span className="dots"><span/><span/><span/></span> thinking…</div>;
  }
  return (
    <div className="typing" onClick={onJump}>
      <span className="dots"><span/><span/><span/></span>
      running <span className="tname mono">{tname}</span>
      <span className="jump">jump →</span>
    </div>
  );
}

function Composer({ onSend, onCancel, running }) {
  const [val, setVal] = uS2('');
  const [slash, setSlash] = uS2(false);
  // Real File objects from <input type="file"> or drag-drop.  Sent as
  // base64 attachments through DysonLive.send → /api/.../turn → agent
  // run_with_attachments (same path Telegram takes for media).
  const [atts, setAtts] = uS2([]);
  const taRef = uR2();
  const fileRef = uR2();
  const cmds = window.DYSON_DATA.slashCmds;
  const filtered = slash ? cmds.filter(c => c.cmd.startsWith(val.split(/\s/)[0] || '/')) : [];

  uE2(() => {
    if (!taRef.current) return;
    taRef.current.style.height = 'auto';
    taRef.current.style.height = Math.min(240, taRef.current.scrollHeight) + 'px';
  }, [val]);

  const sub = (e) => {
    e?.preventDefault();
    if (!val.trim() && !atts.length) return;
    onSend(val, atts);
    setVal(''); setAtts([]); setSlash(false);
  };

  const onPickFiles = (e) => {
    const list = Array.from(e.target.files || []);
    if (list.length) setAtts(a => [...a, ...list]);
    e.target.value = '';  // allow re-picking the same file
  };

  return (
    <div className="composer-wrap">
      {slash && filtered.length > 0 && (
        <div className="slashmenu">
          {filtered.map((c, i) => (
            <div key={i} className={`item ${i===0?'focused':''}`} onClick={() => { setVal(c.cmd + ' '); setSlash(false); taRef.current?.focus(); }}>
              <span className="cmd">{c.cmd}</span>
              <span className="desc">{c.desc}</span>
              <span className="src">{c.src}</span>
            </div>
          ))}
        </div>
      )}
      <div className="composer">
        <input ref={fileRef} type="file" multiple style={{display:'none'}} onChange={onPickFiles}/>
        {atts.length > 0 && (
          <div className="atts">
            {atts.map((a, i) => (
              <span key={i} className="att">
                <Icon name="paperclip" size={10}/> {a.name} <span className="sz">{prettySize(a.size)}</span>
                <span className="x" onClick={() => setAtts(atts.filter((_, j) => j !== i))}>×</span>
              </span>
            ))}
          </div>
        )}
        <textarea
          ref={taRef}
          value={val}
          placeholder={running ? "Dyson is working — this queues" : "Reply to Dyson…"}
          onChange={e => {
            setVal(e.target.value);
            setSlash(e.target.value.startsWith('/'));
          }}
          onKeyDown={e => {
            // Enter alone sends; Shift+Enter inserts a newline.
            // Ignore other modifiers so OS-level shortcuts (⌘↵ etc.)
            // don't double-fire on top of the browser's default.
            if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
              e.preventDefault();
              sub();
            }
            if (e.key === 'Escape') setSlash(false);
          }}
        />
        <div className="row">
          <button className="btn" onClick={() => fileRef.current?.click()} title="Attach files">
            <Icon name="paperclip" size={12}/>
          </button>
          <button className={`btn ${slash?'' : ''}`} onClick={() => { setVal('/'); setSlash(true); taRef.current?.focus(); }} title="Slash menu">
            <Icon name="slash" size={12}/> <span className="btn-label">commands</span>
          </button>
          <span className="sep"/>
          {(window.DYSON_DATA && window.DYSON_DATA.activeModel) && (
            <span className="model-label" style={{fontFamily:'var(--font-mono)', fontSize:10.5, color:'var(--mute)'}}>{window.DYSON_DATA.activeModel}</span>
          )}
          {running ? (
            <button className="btn sm" onClick={onCancel} style={{color:'var(--err)', borderColor:'oklch(0.70 0.21 25 / 0.3)'}}>
              <Icon name="stop" size={11}/> cancel
            </button>
          ) : (
            <button className="btn send sm" onClick={sub} disabled={!val.trim() && !atts.length}>
              send <Kbd>↵</Kbd>
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

function EmptyState({ onSuggest }) {
  // Real values only.  Model from /api/providers, tool count from
  // /api/skills, both injected by bridge.js into window.DYSON_DATA.
  const D = window.DYSON_DATA || {};
  const model = D.activeModel || '';
  const builtinCount = (D.skills && D.skills.builtin && D.skills.builtin.length) || 0;
  const mcpCount = (D.skills && D.skills.mcp && D.skills.mcp.length) || 0;
  const wsBackend = (D.mind && D.mind.backend) || '';
  return (
    <div style={{maxWidth:540, margin:'80px auto 0', padding:'0 32px', color:'var(--fg-dim)'}}>
      <div style={{fontSize:22, fontWeight:500, color:'var(--fg)', marginBottom:6, letterSpacing:'-0.01em'}}>
        You're talking to <span style={{color:'var(--accent)'}}>Dyson</span>.
      </div>
      <div style={{fontSize:13.5, lineHeight:1.55, color:'var(--mute-2)', marginBottom:28}}>
        {model && <>Model <span className="mono" style={{color:'var(--fg-dim)'}}>{model}</span>. </>}
        {builtinCount > 0 && <>{builtinCount} builtin tools{mcpCount > 0 && ` + ${mcpCount} MCP`}. </>}
        {wsBackend && <>Workspace backend <span className="mono" style={{color:'var(--fg-dim)'}}>{wsBackend}</span>.</>}
      </div>
      <div style={{fontSize:13, color:'var(--mute)'}}>Type a message to start.</div>
    </div>
  );
}

function prettySize(n) {
  if (typeof n !== 'number' || n < 0) return '';
  if (n < 1024) return n + 'B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(0) + 'K';
  return (n / 1024 / 1024).toFixed(1) + 'M';
}

// File → base64 (no data-URL prefix).  Used by bridge.js to serialise
// uploads into the JSON turn body.
function fileToBase64(file) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const s = r.result || '';
      const i = typeof s === 'string' ? s.indexOf(',') : -1;
      resolve(i >= 0 ? s.slice(i + 1) : '');
    };
    r.onerror = () => reject(r.error || new Error('read failed'));
    r.readAsDataURL(file);
  });
}

Object.assign(window, { Turn, ThinkingBlock, ToolChip, FileBlock, ArtefactBlock, TypingIndicator, Composer, EmptyState, markdown, prettySize, fileToBase64 });
