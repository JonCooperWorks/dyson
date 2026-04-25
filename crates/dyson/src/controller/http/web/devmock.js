// Dev-only mock for the dyson HTTP controller.  Only loaded when
// VITE_DYSON_MOCK=1.  Lets visual work proceed without spinning up
// the real Rust backend (which needs an LLM provider and sandbox).
// Delete this file when no longer needed.

const NOW = Date.now();
const ago = (s) => new Date(NOW - s * 1000).toISOString();

const CONVERSATIONS = [
  { id: 'sec-audit', title: 'security audit — http batch 4', live: true,  has_artefacts: true,  source: 'http' },
  { id: 'design',    title: 'right-rail spacing pass',        live: false, has_artefacts: false, source: 'http' },
  { id: 'tg-1',      title: 'telegram triage',                live: false, has_artefacts: false, source: 'telegram' },
  { id: 'mind-1',    title: 'workspace memory cleanup',       live: false, has_artefacts: true,  source: 'http' },
  { id: 'plan',      title: 'q2 roadmap brainstorm',          live: false, has_artefacts: false, source: 'http' },
  { id: 'rev',       title: 'subagent prompt iter6',          live: false, has_artefacts: true,  source: 'http' },
  { id: 'web-look',  title: 'make web UI look stunning',      live: true,  has_artefacts: false, source: 'http' },
];

const PROVIDERS = [
  {
    id: 'anthropic', name: 'Anthropic', active: true,
    active_model: 'claude-opus-4-7',
    models: ['claude-opus-4-7', 'claude-sonnet-4-6', 'claude-haiku-4-5'],
  },
  {
    id: 'claude-code', name: 'Claude Code (CLI)', active: false,
    active_model: 'sonnet',
    models: ['sonnet', 'opus', 'haiku'],
  },
  {
    id: 'openai', name: 'OpenAI', active: false,
    active_model: 'gpt-4.1',
    models: ['gpt-4.1', 'gpt-4.1-mini', 'o3'],
  },
];

// Conversation transcripts use the wire format the real backend returns:
//   { messages: [ { role, blocks: [ { type: 'text'|'thinking'|'tool_use'|
//     'tool_result'|'file'|'artefact', ... } ] } ] }
// hydrateTranscript() in app.jsx normalises this into UI shape.
const MESSAGES = {
  'web-look': [
    {
      role: 'user',
      blocks: [{ type: 'text', text: 'make this look absolutely stunning visually. be brave.' }],
    },
    {
      role: 'assistant',
      blocks: [
        { type: 'thinking', thinking: 'They want bold visual changes. The existing palette is solid — OKLCH neutrals on a cool blue, electric lime accent. I should amplify what works: aurora-glow on the accent, glassy depth on the topbar/composer, a richer background texture. No layout changes — visual upgrade only.' },
        { type: 'text', text: "## Visual rework — pass 1\n\nI'm going to push the existing aesthetic *further* rather than rip it out. The skeleton is already refined — OKLCH ramp, electric-lime accent, mono eyebrows. What's missing is **depth, glow, and signature moments**.\n\nHere's the plan:\n\n- **Aurora background** — a calm, slowly-drifting gradient mesh under the centre column. Not distracting; just enough to make the dark feel *alive* instead of flat.\n- **Glow on the accent** — the lime should *radiate*. Buttons, the active conversation rail, the brand mark, the live dot.\n- **Glass topbar** — `backdrop-filter: blur` with a hairline gradient border so it floats above the content.\n- **Composer as instrument** — bigger radius, soft inner-glow on focus, gradient send button that feels like a launch.\n- **Tool chips that pop** — running tools pulse with an accent halo.\n- **Typography polish** — heavier weight contrast, more generous letter-spacing on eyebrows." },
        { type: 'tool_use', id: 'tu_edit_tokens', name: 'Edit', input: { path: 'styles/tokens.css' } },
        { type: 'tool_result', tool_use_id: 'tu_edit_tokens', is_error: false, content: 'updated 14 hunks in styles/tokens.css' },
        { type: 'tool_use', id: 'tu_edit_layout', name: 'Edit', input: { path: 'styles/layout.css' } },
        { type: 'tool_result', tool_use_id: 'tu_edit_layout', is_error: false, content: 'updated 22 hunks in styles/layout.css' },
        { type: 'text', text: "I bumped the panel rounding, added a subtle aurora layer behind the transcript, gave the accent a soft halo, and reworked the composer with a focus glow.\n\n```css\n.composer:focus-within {\n  border-color: var(--accent-ln);\n  box-shadow:\n    0 0 0 1px var(--accent-ln),\n    0 0 32px -8px oklch(0.88 0.22 130 / 0.35),\n    0 12px 32px -12px oklch(0 0 0 / 0.6);\n}\n```\n\n> The aurora is a single fixed `radial-gradient` layer — zero JS, one composited paint. Animation is on `background-position`, GPU-only." },
        { type: 'tool_use', id: 'tu_bash_build', name: 'Bash', input: { command: 'cd web && npm run build' } },
        { type: 'tool_result', tool_use_id: 'tu_bash_build', is_error: false, content: 'built dist/ in 4.2s — 318 KB total' },
      ],
    },
    {
      role: 'user',
      blocks: [{ type: 'text', text: 'love the glow on the composer. push the accent harder on the send button — make it feel like a *launch*.' }],
    },
    {
      role: 'assistant',
      blocks: [
        { type: 'text', text: "On it. I'll layer a conic-gradient sweep behind the send button, lift it on hover with a real shadow, and add a press-to-pulse animation. Coming up." },
        { type: 'tool_use', id: 'tu_sub_design', name: 'subagent', input: { agent: 'frontend_designer', task: 'push accent harder on send button' } },
      ],
    },
  ],
  'sec-audit': [
    {
      role: 'user',
      blocks: [{ type: 'text', text: 'audit the http controller for path traversal in the artefact endpoints' }],
    },
    {
      role: 'assistant',
      blocks: [
        { type: 'thinking', thinking: "I'll spawn a security_engineer subagent and have it review responses.rs for any unchecked path joins." },
        { type: 'text', text: 'Spawning a subagent to do a focused review of `responses.rs::handle_artefact`.' },
        { type: 'tool_use', id: 'tu_sub_sec', name: 'subagent', input: { agent: 'security_engineer', task: 'http controller review' } },
        { type: 'tool_result', tool_use_id: 'tu_sub_sec', is_error: false, content: 'no path traversal found; one finding: error responses leak filesystem paths via Display' },
      ],
    },
  ],
};

const ACTIVITY = {
  'web-look': {
    lanes: [
      { name: 'frontend_designer', status: 'running', note: 'rebuilding tokens.css with aurora layer' },
    ],
  },
  'sec-audit': { lanes: [] },
};

function json(res, body, status = 200) {
  res.statusCode = status;
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify(body));
}

function readBody(req) {
  return new Promise((resolve) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      try { resolve(raw ? JSON.parse(raw) : {}); }
      catch { resolve({}); }
    });
  });
}

export function dysonMock() {
  return {
    name: 'dyson-mock',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const url = req.url || '';
        if (!url.startsWith('/api/')) return next();

        if (url === '/api/auth/config') {
          return json(res, { mode: 'none' });
        }
        if (url === '/api/conversations' && req.method === 'GET') {
          return json(res, CONVERSATIONS);
        }
        if (url === '/api/conversations' && req.method === 'POST') {
          const body = await readBody(req);
          const id = `c-${Date.now().toString(36)}`;
          const row = { id, title: body.title || 'New conversation', live: false, has_artefacts: false, source: 'http' };
          CONVERSATIONS.unshift(row);
          return json(res, row);
        }
        if (url === '/api/providers') return json(res, PROVIDERS);
        if (url === '/api/mind') {
          return json(res, {
            backend: 'filesystem',
            files: [
              { path: 'AGENTS.md',     size: 2_341 },
              { path: 'IDENTITY.md',   size: 1_812 },
              { path: 'HEARTBEAT.md',  size: 982 },
              { path: 'MEMORY.md',     size: 4_704 },
              { path: 'USER.md',       size: 643 },
              { path: 'memory/projects.md', size: 2_010 },
            ],
          });
        }
        const actMatch = url.match(/^\/api\/activity(?:\?chat=(.+))?$/);
        if (actMatch) {
          const chat = actMatch[1] ? decodeURIComponent(actMatch[1]) : null;
          return json(res, chat ? (ACTIVITY[chat] || { lanes: [] }) : { lanes: [] });
        }
        const loadMatch = url.match(/^\/api\/conversations\/([^/?]+)$/);
        if (loadMatch && req.method === 'GET') {
          const id = decodeURIComponent(loadMatch[1]);
          const conv = CONVERSATIONS.find(c => c.id === id);
          if (!conv) return json(res, { error: 'not found' }, 404);
          const messages = MESSAGES[id] || [];
          return json(res, { id: conv.id, title: conv.title, messages, source: conv.source });
        }
        const fbGet = url.match(/^\/api\/conversations\/([^/?]+)\/feedback$/);
        if (fbGet && req.method === 'GET') return json(res, []);
        const artListMatch = url.match(/^\/api\/conversations\/([^/?]+)\/artefacts$/);
        if (artListMatch) return json(res, []);
        const evMatch = url.match(/^\/api\/conversations\/([^/?]+)\/events/);
        if (evMatch) {
          res.setHeader('content-type', 'text/event-stream');
          res.setHeader('cache-control', 'no-cache');
          res.write(': mock SSE\n\n');
          // Keep open; the UI will treat it as live.
          return;
        }
        if (url === '/api/auth/sse-ticket' && req.method === 'POST') {
          res.setHeader('set-cookie', 'dyson_sse=mock; Path=/; HttpOnly; SameSite=Strict');
          return json(res, { ok: true });
        }
        return json(res, { error: 'mock: not implemented', url }, 404);
      });
    },
  };
}
