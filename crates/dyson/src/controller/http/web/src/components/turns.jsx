/* Dyson — turns, subagents, composer */

import React, { useState, useRef, useEffect, useCallback } from 'react';
import { Icon, Kbd } from './icons.jsx';
// The clipboard dance (modern API → legacy textarea + execCommand
// fallback → swallow) used to live inline here and was copy-pasted
// across panels.jsx / views-secondary.jsx.  One implementation now.
import { copyToClipboard } from '../lib/clipboard.js';
import { useAppState } from '../hooks/useAppState.js';
import { useApiOptional } from '../hooks/useApi.js';
import { requestOpenArtefact } from '../store/app.js';
import { FALLBACK_SLASH_COMMANDS } from '../store/constants.js';
import { ToolBody, copyTextForTool, copyInputForTool } from './panels.jsx';
import MarkdownIt from 'markdown-it';

const CLIPBOARD_IMAGE_EXT = {
  'image/png': 'png',
  'image/jpeg': 'jpg',
  'image/gif': 'gif',
  'image/webp': 'webp',
  'image/bmp': 'bmp',
  'image/svg+xml': 'svg',
};

function normalizeClipboardImage(file, index) {
  if (!file || !(file.type || '').startsWith('image/')) return null;
  const hasUsefulName = file.name && !/^image\.(?:png|jpe?g|gif|webp|bmp|svg)$/i.test(file.name);
  if (hasUsefulName) return file;
  const ext = CLIPBOARD_IMAGE_EXT[file.type] || file.type.split('/')[1] || 'png';
  const name = `pasted-image-${index + 1}.${ext.replace(/[^a-z0-9]+/gi, '') || 'png'}`;
  return new File([file], name, {
    type: file.type || 'image/png',
    lastModified: file.lastModified || Date.now(),
  });
}

function clipboardImageFiles(data) {
  if (!data) return [];
  const out = [];
  const seen = new Set();
  const add = (file) => {
    if (!file || !(file.type || '').startsWith('image/')) return;
    const rawSig = `${file.name || ''}:${file.type}:${file.size}:${file.lastModified}`;
    if (seen.has(rawSig)) return;
    const normalized = normalizeClipboardImage(file, out.length);
    if (!normalized) return;
    seen.add(rawSig);
    out.push(normalized);
  };

  for (const item of Array.from(data.items || [])) {
    if (item.kind === 'file' && (item.type || '').startsWith('image/')) {
      add(item.getAsFile());
    }
  }
  for (const file of Array.from(data.files || [])) add(file);
  return out;
}

function commandList(slashCommands) {
  return Array.isArray(slashCommands) && slashCommands.length
    ? slashCommands
    : FALLBACK_SLASH_COMMANDS;
}

function slashCommandPreview(draft, slashCommands = FALLBACK_SLASH_COMMANDS) {
  const text = String(draft || '').trimStart();
  if (!text.startsWith('/')) return null;
  const token = text.split(/\s/)[0];
  if (!token || token === '/') return null;

  const commands = commandList(slashCommands);
  const exact = commands.find(c => c.cmd === token);
  if (exact) {
    const raw = text.slice(token.length).trimStart();
    return {
      state: 'exact',
      cmd: exact.cmd,
      desc: exact.desc || '',
      src: exact.src || 'command',
      tool: exact.tool || null,
      meta: exact.tool ? `direct tool: ${exact.tool}` : `${exact.src || 'controller'} command`,
      raw,
    };
  }

  const matches = commands.filter(c => c.cmd.startsWith(token));
  if (matches.length === 1) {
    const match = matches[0];
    return {
      state: 'partial',
      cmd: match.cmd,
      desc: match.desc || '',
      src: match.src || 'command',
      tool: match.tool || null,
      meta: match.tool ? `direct tool: ${match.tool}` : `${match.src || 'controller'} command`,
      raw: '',
    };
  }
  if (matches.length > 1) {
    return {
      state: 'partial',
      cmd: token,
      desc: matches.slice(0, 4).map(c => c.cmd).join(', '),
      src: 'matches',
      tool: null,
      meta: `${matches.length} matches`,
      raw: '',
    };
  }

  return {
    state: 'unknown',
    cmd: token,
    desc: 'No registered slash command matches this name.',
    src: 'unknown',
    tool: null,
    meta: 'not registered',
    raw: '',
  };
}

const COMPOSER_MOBILE_QUERY = '(max-width: 760px)';
const COMPOSER_DESKTOP_FONT_SIZE = '16px';
const COMPOSER_MOBILE_FONT_SIZE = '17px';

function composerFocusFontSize() {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
    return COMPOSER_DESKTOP_FONT_SIZE;
  }
  return window.matchMedia(COMPOSER_MOBILE_QUERY).matches
    ? COMPOSER_MOBILE_FONT_SIZE
    : COMPOSER_DESKTOP_FONT_SIZE;
}

function composerMobileViewport() {
  return typeof window !== 'undefined'
    && typeof window.matchMedia === 'function'
    && window.matchMedia(COMPOSER_MOBILE_QUERY).matches;
}

function viewportMeta() {
  if (typeof document === 'undefined') return null;
  return document.querySelector('meta[name="viewport"]');
}

function composerLockedViewportContent(base) {
  const cleaned = String(base || 'width=device-width, initial-scale=1')
    .split(',')
    .map(part => part.trim())
    .filter(part => part && !/^maximum-scale\s*=/i.test(part) && !/^user-scalable\s*=/i.test(part));
  cleaned.push('maximum-scale=1');
  return cleaned.join(', ');
}

let composerViewportUnlockTimer = null;

function setComposerViewportLocked(locked) {
  const meta = viewportMeta();
  if (!meta) return;
  if (composerViewportUnlockTimer) {
    clearTimeout(composerViewportUnlockTimer);
    composerViewportUnlockTimer = null;
  }
  const original = meta.dataset.dysonComposerViewport || meta.getAttribute('content') || 'width=device-width, initial-scale=1';
  meta.dataset.dysonComposerViewport = original;
  if (locked) {
    meta.setAttribute('content', composerLockedViewportContent(original));
    return;
  }
  composerViewportUnlockTimer = setTimeout(() => {
    meta.setAttribute('content', meta.dataset.dysonComposerViewport || original);
    delete meta.dataset.dysonComposerViewport;
    composerViewportUnlockTimer = null;
  }, 250);
}

function recenterComposerRootScroll() {
  if (!composerMobileViewport() || typeof document === 'undefined') return;
  const schedule = typeof window.requestAnimationFrame === 'function'
    ? window.requestAnimationFrame.bind(window)
    : (cb) => setTimeout(cb, 0);
  schedule(() => {
    schedule(() => {
      // WebKit may scroll the layout viewport after focus even with a
      // fixed app shell.  Put the root back; the transcript owns scroll.
      if (typeof window.scrollTo === 'function') window.scrollTo(0, 0);
      document.documentElement.scrollTop = 0;
      if (document.body) document.body.scrollTop = 0;
    });
  });
}

function prepareComposerFocus(el) {
  pinComposerFocusGuard(el);
  setComposerViewportLocked(true);
  recenterComposerRootScroll();
}

function releaseComposerFocus() {
  setComposerViewportLocked(false);
}

function pinComposerFocusGuard(el) {
  if (!el) return;
  el.style.setProperty('font-size', composerFocusFontSize(), 'important');
  el.style.setProperty('line-height', '1.5');
  el.style.setProperty('-webkit-text-size-adjust', '100%');
  el.style.setProperty('text-size-adjust', '100%');
  el.style.setProperty('touch-action', 'manipulation');
}

function ThinkingBlock({ text }) {
  return (
    <details className="thinking">
      <summary><span className="caret"><Icon name="chev" size={10}/></span> thinking</summary>
      <div className="body">{text}</div>
    </details>
  );
}

// Header strip for an inline tool block.  Click toggles the expanded
// body underneath (rendered by ToolBlock).  The chevron rotates 90° in
// CSS when the wrapping .toolblock is .expanded so the caret reads as a
// disclosure.
function ToolChip({ tool, onToggle, expanded }) {
  const running = tool.status === 'running';
  const onCopy = async (e) => {
    e.stopPropagation();
    await copyToClipboard(copyTextForTool(tool));
  };
  return (
    <div className={`toolchip ${expanded ? 'active' : ''} ${running ? 'running' : ''}`}
         role="button"
         aria-expanded={expanded ? 'true' : 'false'}
         onClick={onToggle}>
      <span className="icon">{tool.icon}</span>
      <span className="sig"><span className="tname">{tool.name}</span>{tool.sig}</span>
      <span className="meta">
        <span className="dur">{tool.dur}</span>
        <span className={`exit ${tool.exit === 'ok' ? 'ok' : 'err'}`}>
          {running ? '…' : tool.exit === 'ok' ? 'exit 0' : 'exit 1'}
        </span>
      </span>
      {expanded && (
        <button className="tool-copy" onClick={onCopy} title="Copy tool output">
          <Icon name="copy" size={11}/>
        </button>
      )}
      <span className="open">
        <span className="lbl">{expanded ? 'hide' : 'open'}</span>
        <Icon name="chev" size={10}/>
      </span>
    </div>
  );
}

// Inline tool block: chip header + (optionally) the expanded body.
// `expanded` flips when the user clicks the chip OR when the SSE stream
// signals a new live tool via openPanel; both go through the same
// session.panels reducer so the URL deep-link path is the same too.
function ToolBlock({ tool, toolRef, expanded, onToggle }) {
  const running = tool.status === 'running';
  const [copied, setCopied] = useState('');
  const copyToolPart = async (kind, text) => {
    if (!text) return;
    if (await copyToClipboard(text)) {
      setCopied(kind);
      setTimeout(() => setCopied(''), 1200);
    }
  };
  const inputText = copyInputForTool(tool);
  return (
    <div className={`toolblock${expanded ? ' expanded' : ''}${running ? ' live' : ''}`}
         data-tool-ref={toolRef || undefined}>
      <ToolChip tool={tool} onToggle={onToggle} expanded={expanded}/>
      {expanded && (
        <div className="toolblock-body">
          <div className="toolblock-actions">
            {inputText && (
              <button className={copied === 'input' ? 'on' : ''} onClick={() => copyToolPart('input', inputText)}>
                <Icon name={copied === 'input' ? 'rate' : 'copy'} size={11}/> input
              </button>
            )}
            <button className={copied === 'output' ? 'on' : ''} onClick={() => copyToolPart('output', copyTextForTool(tool))}>
              <Icon name={copied === 'output' ? 'rate' : 'copy'} size={11}/> output
            </button>
            <span className="sep"/>
            <button onClick={onToggle}><Icon name="chev" size={10}/> collapse</button>
          </div>
          <ToolBody tool={tool}/>
        </div>
      )}
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

function CostPill({ cost }) {
  if (!cost) return null;
  const hasCost = cost.display_cost_usd !== undefined && cost.display_cost_usd !== null;
  if (!hasCost) return null;
  const label = formatMessageCost(cost.display_cost_usd);
  if (!label) return null;
  const title = [
    cost.provider && `provider: ${cost.provider}`,
    cost.model && `model: ${cost.model}`,
    cost.input_tokens != null && `input: ${formatCompactNumber(cost.input_tokens)} tokens`,
    cost.output_tokens != null && `output: ${formatCompactNumber(cost.output_tokens)} tokens`,
    cost.cost_source && `source: ${cost.cost_source}`,
  ].filter(Boolean).join('\n');
  return <span className="cost-pill" title={title || undefined}>{label}</span>;
}

function formatMessageCost(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return '';
  if (n > 0 && n < 0.01) return `$${n.toFixed(n < 0.0001 ? 6 : 4).replace(/0+$/u, '').replace(/\.$/u, '')}`;
  return `$${n.toFixed(2)}`;
}

function formatCompactNumber(value) {
  const n = Number(value || 0);
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1).replace(/\.0$/u, '')}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1).replace(/\.0$/u, '')}k`;
  return String(Math.max(0, Math.round(n)));
}

function Turn({ turn, tools, onOpenTool, expandedTools, turnIndex, rating, onRate,
                chatId,
                reactionsOpen, onToggleReactions }) {
  const expandedSet = expandedTools instanceof Set
    ? expandedTools
    : new Set(Array.isArray(expandedTools) ? expandedTools : []);
  const isUser = turn.role === 'user';
  const agentName = useAppState(s => s.agentName) || 'dyson';
  // Two-letter avatar pulled from the agent name's first two glyphs
  // (uppercased) so a user-set "Atlas" reads as AT, not DY.  Falls back
  // to DY when agentName came up empty so the existing visual stays put.
  const agentInitials = (agentName.replace(/\s+/g, '').slice(0, 2) || 'dy').toUpperCase();
  const avatarL = isUser ? 'me' : agentInitials;
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
          <span className="name">{isUser ? 'you' : agentName}</span>
          {/* The model rides on the per-turn cost metadata (cost.model),
              which hydrated transcripts carry. The old `turn.model` field
              was never populated anywhere, so this label was dead on
              loaded history — render it from the cost the turn already has. */}
          {!isUser && turn.cost?.model && <span className="model">{turn.cost.model}</span>}
          {!isUser ? <CostPill cost={turn.cost}/> : null}
          {turn.queued && (
            <span
              className="queued-badge"
              title={turn.queuedCount > 1
                ? `${turn.queuedCount} messages queued — ${agentName} will answer them in one reply`
                : turn.queueMode === 'next_tool_call'
                  ? 'Queued for the next tool-call opportunity'
                  : 'Queued behind the in-flight turn'}>
              queued{turn.queueMode === 'next_tool_call' ? ' · next tool' : ''}{turn.queuedCount > 1 ? ` ×${turn.queuedCount}` : ''}
            </span>
          )}
          {turn.nextRunModel?.model && (
            <span className="queued-badge next-model" title="Model selected for this queued run">
              next {turn.nextRunModel.model}
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
            return <ToolBlock key={i} tool={t} toolRef={b.ref}
                              expanded={expandedSet.has(b.ref)}
                              onToggle={() => onOpenTool(b.ref)}/>;
          }
          if (b.type === 'file') {
            return <FileBlock key={i} block={b}/>;
          }
          if (b.type === 'artefact') {
            return <ArtefactBlock key={i} block={b} chatId={chatId}/>;
          }
          if (b.type === 'error') {
            return <ErrorBlock key={i} block={b}/>;
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
// is reached through the chat-scoped artefact body route, which hands
// back the scoped file URL as plain text).  For markdown / report
// artefacts, a compact chip that opens the Artefacts tab on click.
//
// `block.url` is an SPA deep-link (`/#/artefacts/<id>`) — set so that
// cmd-click / copy-paste of the chip lands on the reader instead of
// the raw bytes endpoint.
//
// `block` shape: { type:'artefact', id, kind, title, url, bytes }
function ArtefactBlock({ block, chatId }) {
  const open = (e) => {
    e.preventDefault();
    if (block.id) requestOpenArtefact(block.id);
  };
  // Deep-link to the reader, used as the href fallback so cmd-click
  // always has somewhere sensible to go — never `#` (which would jump
  // the page to the top).
  const reader = block.url || (block.id ? `/#/artefacts/${encodeURIComponent(block.id)}` : '#');
  const [src, setSrc] = useState('');

  useEffect(() => {
    if (block.kind !== 'image' || !block.id || !chatId) {
      setSrc('');
      return;
    }
    let cancelled = false;
    fetch(`/api/conversations/${encodeURIComponent(chatId)}/artefacts/${encodeURIComponent(block.id)}`)
      .then(r => r.ok ? r.text() : Promise.reject(r.status))
      .then(text => { if (!cancelled) setSrc(text.trim()); })
      .catch(() => {});
    return () => { cancelled = true; };
  }, [block.kind, block.id, chatId]);

  if (block.kind === 'image') {
    // For a zero-hop preview we fetch the file URL once on mount and
    // swap to <img>.  Until the fetch lands, cmd-click opens the
    // reader (via `reader`); once resolved, cmd-click opens the image
    // itself in a new tab — which is what users reach for on an image.
    return (
      <div className="artefact-chat-block image">
        <a href={src || reader} target="_blank" rel="noopener" onClick={open}
           className="fileblock image" title={block.title || 'image'}>
          {src
            ? <img src={src} alt={block.title || 'image'}/>
            : <div style={{width:220, height:160, background:'var(--panel)', borderRadius:4}}/>}
          <span className="cap">{block.title || 'image'}</span>
        </a>
      </div>
    );
  }

  const kind = (block.kind || 'other').replace(/_/g, ' ');
  return (
    <div className="artefact-chat-block">
      <a href={reader} onClick={open} className="fileblock" title="Open artefact">
        <Icon name="file" size={14}/>
        <span className="name">{block.title || 'Artefact'}</span>
        <span className="sz mono" style={{color:'var(--fg-dim)'}}>{kind}</span>
        {typeof block.bytes === 'number' && (
          <span className="sz mono">{prettySize(block.bytes)}</span>
        )}
        <span className="dl mono">open →</span>
      </a>
    </div>
  );
}

// Renders a turn-level error (LLM provider failure, tool dispatch
// error, etc.) as a distinct red-tinted card instead of inline text,
// so the user doesn't have to mentally separate "[error] …" from the
// surrounding agent prose.  Block shape: { type:'error', message }.
function ErrorBlock({ block }) {
  return (
    <div className="errorblock" role="alert">
      <span className="errorblock-tag mono">error</span>
      <span className="errorblock-msg">{block.message}</span>
    </div>
  );
}

function RunStatusStrip({ phase, tname, startedAt, onCancel, onJump, onCollapseAll }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  const elapsed = startedAt ? Math.max(0, Math.floor((now - startedAt) / 1000)) : 0;
  const phaseLabel = phase === 'tool' ? 'running tool'
    : phase === 'streaming' ? 'streaming'
    : phase === 'compacting' ? 'compacting context'
    : 'thinking';
  const mm = Math.floor(elapsed / 60).toString();
  const ss = (elapsed % 60).toString().padStart(2, '0');
  return (
    <div className="run-status" role="status" aria-label="Run status">
      <span className="dots"><span/><span/><span/></span>
      <span className="phase">{phaseLabel}</span>
      {phase === 'tool' && tname && <span className="tname mono">{tname}</span>}
      <span className="elapsed mono">{mm}:{ss}</span>
      <span className="sep"/>
      <button className="btn ghost xs" onClick={onJump} title="Jump to latest" aria-label="Jump to latest">
        <Icon name="arr-down" size={11}/> latest
      </button>
      <button className="btn ghost xs" onClick={onCollapseAll} title="Collapse all tool blocks" aria-label="Collapse all tool blocks">
        <Icon name="chev" size={10}/> collapse
      </button>
      <button className="btn ghost xs danger" onClick={onCancel} title="Cancel run" aria-label="Cancel run">
        <Icon name="stop" size={11}/> cancel
      </button>
    </div>
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

// Detect whether an uploaded JSON file is a security_engineer harness
// checkpoint.  The trio of `schema_version`, `harness_version`, and a
// `run_id` matching `sec-*` is decisive — no other workspace JSON we
// emit has all three.  Returns the parsed object or null.
function parseSecurityCheckpoint(text) {
  try {
    const j = JSON.parse(text);
    if (
      j && typeof j === 'object'
      && typeof j.schema_version === 'number'
      && typeof j.harness_version === 'string'
      && typeof j.run_id === 'string'
      && /^sec-[a-z0-9-]+$/i.test(j.run_id)
    ) {
      return j;
    }
  } catch { /* not JSON or malformed — fall through */ }
  return null;
}

function Composer({
  onSend,
  onCancel,
  running,
  autoFocusKey,
  draftText = '',
  draftAttachments = [],
  onDraftChange,
  queueMode = 'normal',
  nextRunModel = null,
  onQueueModeChange,
  slashCommands = FALLBACK_SLASH_COMMANDS,
}) {
  const controlled = typeof onDraftChange === 'function';
  const [localVal, setLocalVal] = useState('');
  const [slash, setSlash] = useState(false);
  // Highlighted row in the slash menu, driven by ArrowUp/ArrowDown and
  // hover.  Mirrors command-palette.jsx's `active` cursor.
  const [slashIdx, setSlashIdx] = useState(0);
  // Real File objects from <input type="file"> or drag-drop.  Sent as
  // base64 attachments through DysonClient.send → /api/.../turn → agent
  // run_with_attachments (same path Telegram takes for media).
  const [localAtts, setLocalAtts] = useState([]);
  // Transient toast for the "checkpoint uploaded" affordance — shows
  // briefly under the composer when an upload landed at
  // kb/security-harness/checkpoints/<run_id>.json so the operator can
  // see WHICH run_id was just made resumable.
  const [checkpointToast, setCheckpointToast] = useState('');
  const apiClient = useApiOptional();
  const taRef = useRef();
  const fileRef = useRef();
  const slashItemRef = useRef(null);
  const activeModel = useAppState(s => s.activeModel);
  const agentName = useAppState(s => s.agentName) || 'dyson';
  const val = controlled ? draftText : localVal;
  const atts = controlled ? draftAttachments : localAtts;
  const availableSlashCommands = commandList(slashCommands);
  const trimmedDraft = val.trimStart();
  const commandToken = trimmedDraft.split(/\s/)[0] || '/';
  const commandTokenOnly = trimmedDraft.startsWith('/') && !/\s/.test(trimmedDraft);
  const filtered = slash && commandTokenOnly ? availableSlashCommands.filter(c => c.cmd.startsWith(commandToken)) : [];
  // Clamp the highlight into range — the filtered list shrinks as the
  // user types, so a stale index must not point past the end.
  const activeSlashIdx = filtered.length ? Math.min(slashIdx, filtered.length - 1) : 0;
  const preview = slashCommandPreview(val, availableSlashCommands);
  const setDraft = useCallback((text, attachments = atts) => {
    if (controlled) onDraftChange({ text, attachments });
    else setLocalVal(text);
  }, [atts, controlled, onDraftChange]);
  const setAttachments = useCallback((next) => {
    const attachments = typeof next === 'function' ? next(atts) : next;
    if (controlled) onDraftChange({ text: val, attachments });
    else setLocalAtts(attachments);
  }, [atts, controlled, onDraftChange, val]);

  const setTextareaRef = useCallback((node) => {
    taRef.current = node;
    pinComposerFocusGuard(node);
  }, []);

  const focusTextarea = useCallback(() => {
    prepareComposerFocus(taRef.current);
    taRef.current?.focus({ preventScroll: true });
  }, []);

  // Insert a slash command into the draft and dismiss the menu.  Shared
  // by click and by Enter/Tab keyboard selection.
  const pickSlash = useCallback((c) => {
    if (!c) return;
    setDraft(c.cmd + ' ');
    setSlash(false);
    focusTextarea();
  }, [setDraft, focusTextarea]);

  // Reset the highlight to the top whenever the filter text changes or
  // the menu (re)opens, matching command-palette's behaviour.
  useEffect(() => { setSlashIdx(0); }, [commandToken, slash]);

  // Keep the highlighted row visible as arrow-nav walks a scrolling list.
  // `?.()` guards jsdom, which doesn't implement scrollIntoView.
  useEffect(() => {
    slashItemRef.current?.scrollIntoView?.({ block: 'nearest' });
  }, [activeSlashIdx, slash]);

  useEffect(() => {
    pinComposerFocusGuard(taRef.current);
    return () => releaseComposerFocus();
  }, []);

  useEffect(() => {
    if (!taRef.current) return;
    taRef.current.style.height = 'auto';
    taRef.current.style.height = Math.min(240, taRef.current.scrollHeight) + 'px';
  }, [val]);

  useEffect(() => {
    if (!autoFocusKey) return;
    const id = setTimeout(focusTextarea, 0);
    return () => clearTimeout(id);
  }, [autoFocusKey, focusTextarea]);

  const sub = (e) => {
    e?.preventDefault();
    if (!val.trim() && !atts.length) return;
    onSend(val, atts);
    if (controlled) onDraftChange({ text: '', attachments: [] });
    else { setLocalVal(''); setLocalAtts([]); }
    setSlash(false);
  };

  // When the operator attaches a file that looks like a
  // security_engineer SecurityCheckpoint JSON, take a special path:
  //   1. POST it to /api/mind/file at the canonical resume path
  //      kb/security-harness/checkpoints/<run_id>.json
  //   2. Prepend a "resume from this checkpoint" prompt to the draft
  //      so the next send fires security_engineer with resume=true
  //   3. Show a transient toast carrying the run_id
  // Everything else falls through to the normal attachment flow
  // (base64 → run_with_attachments).
  const handleSecurityCheckpointUpload = async (file) => {
    let text;
    try { text = await file.text(); }
    catch { return false; }
    const cp = parseSecurityCheckpoint(text);
    if (!cp) return false;
    // No API client in test contexts — fall through to attachment flow.
    if (!apiClient) return false;
    const path = `kb/security-harness/checkpoints/${cp.run_id}.json`;
    try {
      await apiClient.postMindFile(path, text);
    } catch (e) {
      setCheckpointToast(`upload failed: ${(e && e.message) || e}`);
      setTimeout(() => setCheckpointToast(''), 4000);
      return true; // we handled it; don't fall through to attachments
    }
    const stagePart = cp.current_stage ? ` (stage: ${cp.current_stage})` : '';
    const promptLine = `Please resume the security_engineer review from checkpoint \`${cp.run_id}\`${stagePart}. The checkpoint has been written to \`${path}\`.`;
    const prefix = val.trim() ? `${val.trimEnd()}\n\n` : '';
    setDraft(`${prefix}${promptLine}`);
    setCheckpointToast(`uploaded ${cp.run_id} → ${path}`);
    setTimeout(() => setCheckpointToast(''), 6000);
    return true;
  };

  const onPickFiles = async (e) => {
    const list = Array.from(e.target.files || []);
    e.target.value = '';  // allow re-picking the same file (must reset early)
    // Partition: checkpoints get routed to the workspace; everything
    // else goes into the attachment list as before.
    const remaining = [];
    for (const f of list) {
      if (/\.json$/i.test(f.name)) {
        // Only inspect .json files — saves a file.text() round-trip on PNGs.
        const handled = await handleSecurityCheckpointUpload(f);
        if (handled) continue;
      }
      remaining.push(f);
    }
    if (remaining.length) setAttachments(a => [...a, ...remaining]);
  };

  const onPaste = (e) => {
    const images = clipboardImageFiles(e.clipboardData);
    if (!images.length) return;
    e.preventDefault();
    setAttachments(a => [...a, ...images]);
  };

  return (
    <div className="composer-wrap">
      {slash && filtered.length > 0 && (
        <div className="slashmenu">
          {filtered.map((c, i) => (
            <div key={i}
                 ref={i === activeSlashIdx ? slashItemRef : null}
                 className={`item ${i === activeSlashIdx ? 'focused' : ''}`}
                 onMouseEnter={() => setSlashIdx(i)}
                 onClick={() => pickSlash(c)}>
              <span className="cmd">{c.cmd}</span>
              <span className="desc">{c.desc}</span>
              <span className="src">{c.src}</span>
            </div>
          ))}
        </div>
      )}
      {checkpointToast && (
        <div data-testid="checkpoint-toast"
             style={{padding:'6px 12px', fontSize:11,
                     background: /failed|error/i.test(checkpointToast) ? 'var(--err-dim, #4a1f1f)' : 'var(--ok-dim, #1f4a2a)',
                     color:'var(--fg)',
                     borderTop:'1px solid var(--line)',
                     fontFamily:'var(--font-mono)'}}>
          {checkpointToast}
        </div>
      )}
      <div className="composer">
        <input ref={fileRef} type="file" multiple style={{display:'none'}} onChange={onPickFiles}/>
        {atts.length > 0 && (
          <div className="atts">
            {atts.map((a, i) => (
              <span key={i} className="att">
                <Icon name="paperclip" size={10}/> {a.name} <span className="sz">{prettySize(a.size)}</span>
                <span className="x" onClick={() => setAttachments(atts.filter((_, j) => j !== i))}>×</span>
              </span>
            ))}
          </div>
        )}
        {preview && (
          <div className={`slash-preview ${preview.state}`} data-testid="slash-preview">
            <div className="slash-preview-head">
              <span className="cmd">{preview.cmd}</span>
              <span className="src">{preview.src}</span>
              {preview.tool && <span className="tool">{preview.tool}</span>}
            </div>
            <div className="desc">{preview.desc}</div>
            <div className="meta">
              <span>{preview.meta}</span>
              {preview.raw && <span className="raw">{preview.raw}</span>}
            </div>
          </div>
        )}
        <textarea
          ref={setTextareaRef}
          className="composer-input"
          value={val}
          placeholder={running ? `${agentName} is working — this queues` : `Reply to ${agentName}…`}
          onTouchStart={e => prepareComposerFocus(e.currentTarget)}
          onPointerDown={e => prepareComposerFocus(e.currentTarget)}
          onFocus={e => prepareComposerFocus(e.currentTarget)}
          onBlur={releaseComposerFocus}
          onChange={e => {
            setDraft(e.target.value);
            setSlash(e.target.value.trimStart().startsWith('/'));
          }}
          onPaste={onPaste}
          onKeyDown={e => {
            const menuOpen = slash && filtered.length > 0;
            // Arrow keys drive the slash-menu highlight while it's open.
            if (menuOpen && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
              e.preventDefault();
              setSlashIdx(() => e.key === 'ArrowDown'
                ? Math.min(activeSlashIdx + 1, filtered.length - 1)
                : Math.max(activeSlashIdx - 1, 0));
              return;
            }
            // Tab also picks the highlighted command (no newline cost).
            if (menuOpen && e.key === 'Tab' && !e.shiftKey) {
              e.preventDefault();
              pickSlash(filtered[activeSlashIdx]);
              return;
            }
            // Enter alone sends; Shift+Enter inserts a newline.
            // Ignore other modifiers so OS-level shortcuts (⌘↵ etc.)
            // don't double-fire on top of the browser's default.  While
            // the slash menu is open, Enter picks the highlighted command
            // instead of sending.
            if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
              e.preventDefault();
              if (menuOpen) { pickSlash(filtered[activeSlashIdx]); return; }
              sub();
              return;
            }
            if (e.key === 'Escape') setSlash(false);
          }}
        />
        <div className="row">
          <button className="btn" onClick={() => fileRef.current?.click()} title="Attach files" aria-label="Attach files">
            <Icon name="paperclip" size={12}/>
          </button>
          <button className={`btn ${slash?'' : ''}`} onClick={() => { setDraft('/'); setSlash(true); focusTextarea(); }} title="Slash menu" aria-label="Slash commands">
            <Icon name="slash" size={12}/> <span className="btn-label">commands</span>
          </button>
          {running && (
            <button
              className={`btn queue-toggle ${queueMode === 'next_tool_call' ? 'on' : ''}`}
              onClick={() => onQueueModeChange?.(queueMode === 'next_tool_call' ? 'normal' : 'next_tool_call')}
              aria-pressed={queueMode === 'next_tool_call'}
              title="Queue with next tool call" aria-label="Queue with next tool call">
              <Icon name="arr-right" size={11}/> next tool
            </button>
          )}
          <span className="sep"/>
          {running && nextRunModel?.model && (
            <span className="model-label next-run-label">next {nextRunModel.model}</span>
          )}
          {activeModel && (
            <span className="model-label" style={{fontFamily:'var(--font-mono)', fontSize:10.5, color:'var(--mute)'}}>{activeModel}</span>
          )}
          <button className="btn send sm" onClick={sub} disabled={!val.trim() && !atts.length}
                  aria-label={running ? 'Queue message' : 'Send message'}>
            {running ? 'queue' : 'send'} <Kbd>↵</Kbd>
          </button>
        </div>
      </div>
    </div>
  );
}

function EmptyState() {
  // Real values only.  Model from /api/providers, mind backend from
  // /api/mind, MCP names from /api/agent — all live in the app store.
  const model = useAppState(s => s.activeModel);
  const mind = useAppState(s => s.mind);
  const agentName = useAppState(s => s.agentName) || 'Dyson';
  const mcpServers = useAppState(s => s.skills?.mcp || []);
  const stateSync = useAppState(s => s.stateSync);
  const wsBackend = (mind && mind.backend) || '';
  const mcpNames = mcpServers
    .map(s => String(s?.name || '').trim())
    .filter(Boolean);
  const shownMcp = mcpNames.slice(0, 3).join(', ');
  const hiddenMcp = Math.max(0, mcpNames.length - 3);
  return (
    <div className="empty-state">
      <div className="es-eyebrow">
        <span className="es-dot"/>
        <span>online · ready</span>
      </div>
      <h1>You're talking to <em>{agentName}</em>.</h1>
      <p>
        {model && <>Model <span className="mono es-pill">{model}</span>. </>}
        {mcpNames.length > 0 && <>MCP <span className="mono es-pill">{shownMcp}{hiddenMcp > 0 ? `, +${hiddenMcp}` : ''}</span>. </>}
        {stateSync?.configured && stateSync?.last_error && <>Memory sync <span className="mono es-pill">error</span>. </>}
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

export { Turn, ThinkingBlock, ToolChip, ToolBlock, FileBlock, ArtefactBlock, ErrorBlock, RunStatusStrip, TypingIndicator, Composer, EmptyState, markdown, prettySize, fileToBase64, clipboardImageFiles, slashCommandPreview, composerFocusFontSize, composerLockedViewportContent, pinComposerFocusGuard, prepareComposerFocus, releaseComposerFocus };
