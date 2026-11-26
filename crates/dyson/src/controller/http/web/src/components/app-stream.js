/* SSE stream reducers and transcript hydration for the conversation
 * view.  Every callback is a reducer over the sessions / tool stores. */

import {
  setTool, updateTool, mergeTools, markConversationHasArtefacts, upsertConversation,
} from '../store/app.js';
import {
  updateSession, getSession, getResources, mintToolRef,
  mapAgentTail, appendAgentBlock, settleRun,
  admitUserMessage, openPanel,
} from '../store/sessions.js';

export const RATING_EMOJI = {
  terrible: '💩', bad: '👎', not_good: '😐',
  good: '👍', very_good: '🔥', excellent: '❤️',
};

export function setRating(s, turnIndex, emoji) {
  const next = { ...s.ratings };
  if (emoji) next[turnIndex] = emoji; else delete next[turnIndex];
  return { ...s, ratings: next };
}

// -- SSE callbacks ------------------------------------------------------
// Each callback is a reducer over the session store (for transcript +
// panels) or the tool store (for tool views shared across components).
// Helpers live in store/sessions.js so the flow is composable: every
// mutation returns a new frozen snapshot.
export function streamCallbacks(conv) {
  const appendToolText = (ref, extra) => updateTool(ref,
    t => ({ ...t, body: { ...t.body, text: (t.body.text || '') + extra } }));

  return {
    onText: (delta) => updateSession(conv, s => ({
      ...mapAgentTail(s, t => {
        const tail = t.blocks[t.blocks.length - 1];
        return tail?.type === 'text'
          ? { ...t, blocks: [...t.blocks.slice(0, -1), { ...tail, text: tail.text + delta }] }
          : { ...t, blocks: [...t.blocks, { type: 'text', text: delta }] };
      }),
      phase: 'streaming',
    })),

    onThinking: (delta) => {
      const existing = getSession(conv)?.thinkingRef;
      if (existing) return appendToolText(existing, delta);
      // One thinking panel per turn; all deltas land in the same ref.
      const ref = mintToolRef(conv, 'thinking');
      setTool(ref, mkTool('thinking', {
        kind: 'thinking', status: 'running', dur: '…', body: { text: delta },
      }));
      updateSession(conv, s => ({
        ...openPanel(appendAgentBlock(s, { type: 'tool', ref }), ref),
        thinkingRef: ref,
      }));
    },

    onToolStart: ({ id, name, parent_tool_id }) => {
      // Nested subagent tool call — attach to the parent panel as a
      // child chip instead of minting a new top-level chip.  Drop on
      // the floor if the parent panel isn't mounted (race on reload);
      // the user will still see the subagent's outer chip, just not
      // the nested progress.
      if (parent_tool_id) {
        updateTool(parent_tool_id, t => {
          const existing = Array.isArray(t.body?.children) ? t.body.children : [];
          const child = { id: id || `child-${existing.length + 1}`, name, status: 'running', dur: '…' };
          return { ...t, kind: 'subagent', body: { ...(t.body || {}), children: [...existing, child] } };
        });
        return;
      }
      const ref = id || mintToolRef(conv, 'live');
      // Dedupe: when a mid-turn reload reattaches the SSE stream, the
      // server's replay ring re-emits every tool_start for the
      // in-flight turn.  Hydrate already minted a `{ type: 'tool',
      // ref }` block from the persisted transcript for any tool
      // call already on disk, so appending again here would render
      // the same tool twice — a stale "running" twin above the live
      // result.  If the ref is already on the agent turn, leave the
      // block alone and just refresh the panel/phase markers.
      const alreadyOnTimeline = (() => {
        const s = getSession(conv);
        if (!s) return false;
        for (let i = s.liveTurns.length - 1; i >= 0; i--) {
          const t = s.liveTurns[i];
          if (t.role !== 'agent') continue;
          return t.blocks.some(b => b.type === 'tool' && b.ref === ref);
        }
        return false;
      })();
      if (!alreadyOnTimeline) setTool(ref, mkTool(name, { status: 'running', dur: '…' }));
      updateSession(conv, s => ({
        ...openPanel(alreadyOnTimeline ? s : appendAgentBlock(s, { type: 'tool', ref }), ref),
        phase: 'tool', tname: name, liveToolRef: ref,
      }));
    },

    onToolResult: ({ content, is_error, view, parent_tool_id, tool_use_id }) => {
      // Nested subagent tool result — find the matching child by id
      // (the backend always tags nested results with `tool_use_id`,
      // including for parallel dispatches).  No fallback: a missing
      // id or an unmatched id means we can't safely route the result,
      // so we drop it rather than guess and corrupt the wrong child.
      if (parent_tool_id) {
        if (!tool_use_id) return;
        updateTool(parent_tool_id, t => {
          const children = Array.isArray(t.body?.children) ? t.body.children : [];
          const idx = children.findIndex(c => c.id === tool_use_id);
          if (idx < 0) return t;
          // Strip the `kind` field out of `view` (it lives on the entry,
          // not the body) without an IIFE allocation.
          let body;
          let kind = children[idx].kind;
          if (view) {
            const { kind: viewKind, ...rest } = view;
            kind = viewKind;
            body = rest;
          } else {
            body = { text: content };
          }
          const nextChildren = children.slice();
          nextChildren[idx] = { ...children[idx],
            status: 'done',
            exit: is_error ? 'err' : 'ok',
            kind, body,
          };
          return { ...t, body: { ...t.body, children: nextChildren } };
        });
        return;
      }
      const ref = getSession(conv)?.liveToolRef;
      if (!ref) return;
      // image_generate's content is `Generated N image(s) for: "…"` —
      // capture the prompt so ImagePanel can surface it alongside the
      // picture.  File event arrives later and overwrites the body.
      const prompt = typeof content === 'string'
        ? content.match(/^Generated \d+ image\(s\) for: "(.+)"$/s)?.[1]
        : null;
      updateTool(ref, t => {
        const next = applyToolView(t, content, is_error, view);
        return prompt && next.name === 'image_generate' ? { ...next, prompt } : next;
      });
    },

    onCheckpoint: ({ text }) => {
      const ref = getSession(conv)?.liveToolRef;
      if (ref) appendToolText(ref, text + '\n');
    },

    // Server announces it's about to auto-compact context. Show a
    // transient "compacting context…" status; the next text/tool
    // event flips phase back to its normal value and clears it.
    onCompacting: () => updateSession(conv, s => ({ ...s, phase: 'compacting' })),

    onTitle: ({ title }) => {
      if (title) upsertConversation({ id: conv, title });
    },

    onError: (message) => updateSession(conv,
      s => appendAgentBlock(s, { type: 'error', message })),

    onFile: ({ name, mime_type, url, inline_image }) => {
      updateSession(conv, s => appendAgentBlock(s, {
        type: 'file', name, mime: mime_type, url, inline: !!inline_image,
      }));
      // For images: also flip the active tool panel into image-kind so
      // the right-rail shows the picture without scrolling chat.
      if (inline_image) {
        const ref = getSession(conv)?.liveToolRef;
        if (ref) updateTool(ref, t => ({
          ...t, kind: 'image',
          body: { url, name, mime: mime_type, prompt: t.prompt || '' },
        }));
      }
    },

    onUserMessage: ({ blocks }) => {
      const ts = new Date().toTimeString().slice(0, 8);
      updateSession(conv, s => admitUserMessage(s, { ts, blocks: blocks || [] }));
    },

    onArtefact: ({ id, kind, title, url, bytes, metadata }) => {
      updateSession(conv, s => {
        // Image artefacts double up on screen during live turns — the
        // preceding `file` event already rendered an <img>.  Skip the
        // transcript chip for images; still track in session.artefacts
        // so the Artefacts tab lists them.
        const withBlock = kind === 'image'
          ? s
          : appendAgentBlock(s, { type: 'artefact', id, kind, title, url, bytes });
        return {
          ...withBlock,
          artefacts: [
            { id, kind, title, bytes, created_at: Math.floor(Date.now() / 1000), metadata },
            ...s.artefacts,
          ],
        };
      });
      markConversationHasArtefacts(conv);
    },

    onDone: () => {
      const ref = getSession(conv)?.thinkingRef;
      if (ref) updateTool(ref, t => ({ ...t, status: 'done', dur: '' }));
      // Mark that any subsequent delta belongs to a new agent run --
      // this is the queue-drain case where the server processes the
      // next queued message right after this Done.  `mapAgentTail`
      // mints a fresh agent turn when it sees `nextAgentNew`, so the
      // drain reply doesn't graft onto the just-finished turn.
      updateSession(conv, s => settleRun(s, { done: true }));
      getResources(conv).es = null;
    },
  };
}

// -- Transcript hydration ----------------------------------------------
// Consumes the /api/conversations/:id response and seeds the session
// store and the tool view dict.
export function hydrateTranscript(conv, data) {
  const mintCounter = () => { const r = getResources(conv); r.counter += 1; return r.counter; };
  const tools = {};

  const blockOf = (b) => {
    switch (b.type) {
      case 'text':     return { type: 'text', text: b.text };
      case 'thinking': return { type: 'thinking', text: b.thinking };
      case 'file':     return { type: 'file', name: b.name, mime: b.mime, size: b.bytes, url: b.url, inline: !!b.inline_image };
      case 'tool_use': {
        const id = b.id || `${conv}-tu-${mintCounter()}`;
        tools[id] = tools[id] || mkTool(b.name);
        if (b.name === 'image_generate' && b.input?.prompt) tools[id].prompt = b.input.prompt;
        return { type: 'tool', ref: id };
      }
      case 'tool_result': {
        const t = tools[b.tool_use_id];
        if (t) Object.assign(t, { status: 'done', exit: b.is_error ? 'err' : 'ok', body: { text: b.content } });
        return null;
      }
      case 'artefact': {
        // Image artefact with a known tool_use_id: flip the matching
        // tool panel into image-kind so the right-rail shows the
        // picture instead of text.
        if (b.kind === 'image' && b.tool_use_id && tools[b.tool_use_id]) {
          const t = tools[b.tool_use_id];
          t.kind = 'image';
          t.body = {
            url: b.metadata?.file_url || b.url,
            name: b.metadata?.file_name || b.title,
            mime: 'image/*',
            prompt: t.prompt || '',
          };
        }
        return { type: 'artefact', id: b.id, kind: b.kind, title: b.title, url: b.url, bytes: b.bytes, tool_use_id: b.tool_use_id, metadata: b.metadata };
      }
      default: return null;
    }
  };

  const liveTurns = (data.messages || [])
    .map(m => ({
      role: m.role === 'user' ? 'user' : 'agent',
      ts: '',
      blocks: m.blocks.map(blockOf).filter(Boolean),
      cost: m.cost || null,
    }))
    .filter(t => t.blocks.length > 0);

  const allBlocks = liveTurns.flatMap(t => t.blocks);
  mergeTools(tools);
  const artefacts = allBlocks
    .filter(b => b.type === 'artefact')
    .map(b => ({ id: b.id, kind: b.kind, title: b.title, bytes: b.bytes, created_at: 0, metadata: b.metadata }));
  updateSession(conv, s => ({ ...s, liveTurns, artefacts }));
}

// Open the SSE stream for an in-flight chat without POSTing a turn,
// and prepare the local session to receive the deltas.  Called by
// the hydrate effect when `data.live` is true (mid-stream reload).
//
// Steps:
//   1. Append an empty agent placeholder iff the last turn isn't
//      already an agent turn — onText appends to the last block of
//      the last turn, so it needs an agent target to write into.
//   2. Flip running so the typing indicator paints.
//   3. Open the SSE stream via client.attach (no /turn POST → no 409).
export function attachLiveStream(conv, client) {
  updateSession(conv, s => {
    const last = s.liveTurns[s.liveTurns.length - 1];
    const liveTurns = last && last.role === 'agent'
      ? s.liveTurns
      : [...s.liveTurns, { role: 'agent', ts: '', blocks: [{ type: 'text', text: '' }] }];
    return { ...s, liveTurns, running: true, phase: 'thinking', runStartedAt: Date.now() };
  });
  return client.attach(conv, streamCallbacks(conv));
}

export function mkTool(name, over = {}) {
  return {
    name,
    icon: ((name && name[0]) || '?').toUpperCase(),
    sig: '', dur: '', exit: 'ok', status: 'done', kind: 'fallback',
    body: { text: '' },
    ...over,
  };
}

// Apply a typed ToolView (SSE wire shape) to a tool entry, producing a
// new frozen value.  Pure — called as an updateTool reducer argument.
//
// Subagent special case: when the panel was already flipped to
// kind='subagent' by inner tool_use_start events streaming in during
// the subagent's run, preserve `body.children` and stash the final
// summary text as `body.summary`.  Without this guard, the typed view
// (or the fallback `{ text: content }`) would clobber the live list of
// child chips users have been watching populate for minutes.
export function applyToolView(t, content, isError, view) {
  const base = { ...t, status: 'done', exit: isError ? 'err' : 'ok' };
  if (t.kind === 'subagent' && Array.isArray(t.body?.children)) {
    return { ...base, body: { ...t.body, summary: content } };
  }
  if (!view?.kind) return { ...base, kind: 'fallback', body: { text: content } };
  const { kind: _k, ...body } = view;
  const next = { ...base, kind: view.kind, body };
  if (view.kind === 'bash' && typeof view.duration_ms === 'number') {
    next.dur = view.duration_ms < 1000
      ? `${view.duration_ms}ms`
      : `${(view.duration_ms / 1000).toFixed(1)}s`;
  }
  if (view.kind === 'diff' && view.files?.[0]) {
    next.sig = view.files[0].path;
    next.meta = `+${view.files[0].add} −${view.files[0].rem}`;
  }
  if (view.kind === 'read' && view.path) next.sig = view.path;
  return next;
}
