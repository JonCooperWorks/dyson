/* Dyson — turns, subagents, composer */

import React, { useState, useRef, useEffect } from 'react';
import { Icon, Kbd } from './icons.jsx';
// The clipboard dance (modern API → legacy textarea + execCommand
// fallback → swallow) used to live inline here and was copy-pasted
// across panels.jsx / views-secondary.jsx.  One implementation now.
import { copyToClipboard } from '../lib/clipboard.js';
import { useAppState } from '../hooks/useAppState.js';
import { requestOpenArtefact } from '../store/app.js';
import { SLASH_COMMANDS } from '../store/constants.js';
import MarkdownIt from 'markdown-it';

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


function Turn({ turn, tools, onOpenTool, activeTool, turnIndex, rating, onRate,
                reactionsOpen, onToggleReactions }) {
  const isUser = turn.role === 'user';
  const avatarL = isUser ? 'JC' : 'DY';
  const ratable = !isUser && turnIndex != null && typeof onRate === 'function';
  const [copied, setCopied] = useState(false);

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
    const ok = await copyToClipboard(turnToText(turn));
    if (!ok) return;
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  const cls = `turn ${ratable ? 'ratable' : ''} ${reactionsOpen ? 'reactions-open' : ''}`.trim();
  return (
    <div className={cls} onPointerUp={onTurnPointerUp}>
      <div className={`avatar ${isUser ? 'user' : 'agent'}`}>{avatarL}</div>
      <div className="col">
        {/* Copy button lives outside `.who` so it can be `position:
            sticky` against the .col flex container — that keeps it
            pinned in the top-right corner of the turn while you scroll
            through long agent messages, without dragging the entire
            header bar across the prose. */}
        <button
          className={`copy-turn ${copied ? 'on' : ''}`}
          onClick={onCopy}
          title={copied ? 'Copied' : 'Copy message'}>
          <Icon name={copied ? 'rate' : 'copy'} size={11}/>
        </button>
        <div className="who">
          <span className="name">{isUser ? 'jcooper' : 'dyson'}</span>
          {turn.model && <span className="model">{turn.model}</span>}
          {turn.queued && (
            <span
              className="queued-badge"
              title={turn.queuedPosition
                ? `Queued behind the in-flight turn (#${turn.queuedPosition})`
                : 'Queued behind the in-flight turn'}>
              queued{turn.queuedPosition ? ` #${turn.queuedPosition}` : ''}
            </span>
          )}
          <span className="when">{turn.ts}</span>
          {rating && <span className="rating-badge" title={`rated ${rating}`}>{rating}</span>}
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

// Markdown renderer — CommonMark + GFM tables via markdown-it.
// html:false escapes raw HTML in source so only parser-emitted tags
// reach the DOM (matches the prior hand-rolled parser's security
// model). breaks:true preserves single-newline <br/> behaviour for
// chat turns. linkify:false — we only auto-link explicit
// [text](url), bare URLs stay as text.
const mdParser = new MarkdownIt({
  html: false,
  breaks: true,
  linkify: false,
  typographer: false,
  langPrefix: 'lang-',
});

// https-only link allowlist — the prior parser enforced this so
// the chat UI can't surface javascript:/data: URLs from agent output.
// Fragment and root-relative links stay.
const defaultValidateLink = mdParser.validateLink.bind(mdParser);
mdParser.validateLink = (url) => {
  const t = String(url).trim().toLowerCase();
  if (t.startsWith('https://') || t.startsWith('#') || t.startsWith('/')) {
    return defaultValidateLink(url);
  }
  return false;
};

function markdown(s) {
  if (!s) return '';
  // Strip hallucinated `![alt](sandbox:///…)` / `![alt](attachment:…)`
  // markdown the model emits alongside a real image_generate tool
  // call — the "URL" isn't fetchable and the raw syntax leaks
  // into chat. Image delivery always comes through the tool's file
  // event, not inline markdown. Must run pre-parse so the broken URL
  // never reaches markdown-it.
  const cleaned = s.replace(/!\[[^\]]*\]\((?:sandbox:|attachment:)[^)]*\)/gi, '');
  return mdParser.render(cleaned);
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
    if (block.id) requestOpenArtefact(block.id);
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
    const [src, setSrc] = useState('');
    useEffect(() => {
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
  const [val, setVal] = useState('');
  const [slash, setSlash] = useState(false);
  // Real File objects from <input type="file"> or drag-drop.  Sent as
  // base64 attachments through DysonClient.send → /api/.../turn → agent
  // run_with_attachments (same path Telegram takes for media).
  const [atts, setAtts] = useState([]);
  const taRef = useRef();
  const fileRef = useRef();
  const activeModel = useAppState(s => s.activeModel);
  const filtered = slash ? SLASH_COMMANDS.filter(c => c.cmd.startsWith(val.split(/\s/)[0] || '/')) : [];

  useEffect(() => {
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
          {activeModel && (
            <span className="model-label" style={{fontFamily:'var(--font-mono)', fontSize:10.5, color:'var(--mute)'}}>{activeModel}</span>
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

function EmptyState() {
  // Real values only.  Model from /api/providers, mind backend from
  // /api/mind — both live in the app store.  Skills isn't populated by
  // the current controller; left as-is so the count stays 0 until the
  // endpoint exists.
  const model = useAppState(s => s.activeModel);
  const mind = useAppState(s => s.mind);
  const wsBackend = (mind && mind.backend) || '';
  const builtinCount = 0;
  const mcpCount = 0;
  return (
    <div className="empty-state">
      <div className="es-eyebrow">
        <span className="es-dot"/>
        <span>online · ready</span>
      </div>
      <h1>You're talking to <em>Dyson</em>.</h1>
      <p>
        {model && <>Model <span className="mono es-pill">{model}</span>. </>}
        {builtinCount > 0 && <>{builtinCount} builtin tools{mcpCount > 0 && ` + ${mcpCount} MCP`}. </>}
        {wsBackend && <>Workspace backend <span className="mono es-pill">{wsBackend}</span>.</>}
      </p>
      <div className="es-hint">Type a message to start.</div>
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

export { Turn, ThinkingBlock, ToolChip, FileBlock, ArtefactBlock, TypingIndicator, Composer, EmptyState, markdown, prettySize, fileToBase64 };
