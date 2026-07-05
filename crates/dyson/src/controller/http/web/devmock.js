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

// Structured report document a completed security_engineer run persists to
// the workspace kb/ tree.  Served through the /api/mind/file mock below so
// SecurityReportView's fetch path works end-to-end in dev.
const SECURITY_REPORT_DOC = {
  schema_version: 1,
  run_id: 'sec-1751700000-42',
  target: { repo_path: '/var/lib/dyson/workspace/programs/vuln-demo', git_ref: '2f9c1ab' },
  scope: 'whole repository, network-reachable entry points prioritized',
  model: { provider: 'openrouter', model: 'deepseek/deepseek-v4-pro' },
  harness_version: 'v3',
  created_at: 1751700000,
  updated_at: 1751702400,
  report_source: 'valid',
  summary: { critical: 1, high: 2, medium: 2, low: 2, new: 4, recurring: 3 },
  findings: [
    {
      id: 'F1', run_finding_id: 'F1', key: 'DYS-1A2B3C4D', recurring: true, occurrences: 4,
      title: 'OS command injection via ping host parameter', severity: 'critical',
      vulnerability_class: 'injection_unsafe_execution',
      trust_boundary: 'remote unauthenticated attacker to host shell',
      entry_point: 'POST /ping (ping_host view)',
      sink_or_decision: 'app.py:27 os.system invoked with shell string',
      root_cause: 'user-controlled host concatenated into a shell command with no argv separation',
      affected_paths: ['vuln-demo/app.py:27', 'vuln-demo/templates/ping.html:12'],
      evidence: [
        'app.py:27 — os.system("ping -c 1 " + request.form["host"])',
        'curl -d host=";id" /ping returns uid=33(www-data) in the response body',
      ],
      reachability: 'reachable',
      tenant_or_instance_impact: 'full host compromise from an unauthenticated request',
      severity_rationale: 'unauthenticated remote command execution, no mitigations in path',
      fix_recommendation: 'use subprocess.run with a list argv and validate the host against a strict pattern',
      suggested_patch: '--- a/app.py\n+++ b/app.py\n@@ -25,3 +25,4 @@\n-    os.system("ping -c 1 " + request.form["host"])\n+    host = request.form["host"]\n+    if not re.fullmatch(r"[A-Za-z0-9.-]{1,253}", host): abort(400)\n+    subprocess.run(["ping", "-c", "1", host], check=False)',
    },
    {
      id: 'F2', run_finding_id: 'F2', key: 'DYS-9F8E7D6C', recurring: true, occurrences: 2,
      title: 'IDOR on user profile lookup', severity: 'high',
      vulnerability_class: 'auth_authorization',
      trust_boundary: 'any authenticated caller to user data store',
      entry_point: 'GET /users/<id> (users_show view)',
      sink_or_decision: 'app.py:14 user row returned without owner check',
      root_cause: 'the view never verifies the session owns the requested id',
      affected_paths: ['vuln-demo/app.py:14'],
      evidence: ['app.py:14 — return jsonify(USERS[int(user_id)])'],
      reachability: 'reachable',
      tenant_or_instance_impact: 'cross-tenant read of any user profile',
      severity_rationale: 'horizontal privilege escalation with trivial enumeration',
      fix_recommendation: 'scope the lookup to session.user_id or add an ownership predicate',
      suggested_patch: '',
    },
    {
      id: 'F3', run_finding_id: 'F3', key: 'DYS-55AA66BB', recurring: false, occurrences: 1,
      title: 'SSRF in webhook preview fetcher', severity: 'high',
      vulnerability_class: 'ssrf_outbound_network',
      trust_boundary: 'authenticated user to internal network',
      entry_point: 'POST /webhooks/preview',
      sink_or_decision: 'fetcher.py:41 requests.get(url) with no destination policy',
      root_cause: 'attacker-supplied URL fetched server-side without an allowlist or metadata-IP block',
      affected_paths: ['vuln-demo/fetcher.py:41'],
      evidence: ['fetcher.py:41 — resp = requests.get(payload["url"], timeout=5)'],
      reachability: 'reachable',
      tenant_or_instance_impact: 'reads cloud metadata and intranet services from the app host',
      severity_rationale: 'server-side pivot into the private network',
      fix_recommendation: 'resolve and validate the destination against a public-IP-only policy before fetching',
      suggested_patch: '',
    },
    {
      id: 'F4', run_finding_id: 'F4', key: 'DYS-0C1D2E3F', recurring: true, occurrences: 5,
      title: 'Session cookie missing Secure and HttpOnly flags', severity: 'medium',
      vulnerability_class: 'session_oauth_csrf',
      trust_boundary: 'network attacker to session token',
      entry_point: 'app.py:8 session cookie configuration',
      sink_or_decision: 'app.py:8 SESSION_COOKIE_SECURE unset',
      root_cause: 'default cookie flags left in place',
      affected_paths: ['vuln-demo/app.py:8'],
      evidence: [],
      reachability: 'requires network position',
      tenant_or_instance_impact: 'session hijack over plaintext hops or via XSS',
      severity_rationale: 'mitigated by TLS at the edge but undefended in depth',
      fix_recommendation: 'set SESSION_COOKIE_SECURE, SESSION_COOKIE_HTTPONLY, SameSite=Lax',
      suggested_patch: '',
    },
    {
      id: 'F5', run_finding_id: 'F5', key: 'DYS-77CC88DD', recurring: false, occurrences: 1,
      title: 'Path traversal in static file download', severity: 'medium',
      vulnerability_class: 'path_traversal_file_access',
      trust_boundary: 'unauthenticated caller to app filesystem',
      entry_point: 'GET /files/<name>',
      sink_or_decision: 'app.py:52 open(os.path.join(BASE, name))',
      root_cause: 'no normalization or containment check on the joined path',
      affected_paths: ['vuln-demo/app.py:52'],
      evidence: ['app.py:52 — open(os.path.join(BASE, request.args["name"]))',
        'GET /files/..%2f..%2fetc/passwd returns the host password file'],
      reachability: 'reachable',
      tenant_or_instance_impact: 'arbitrary file read from the app working directory',
      severity_rationale: 'unauthenticated read but confined to files the process can open',
      fix_recommendation: 'reject any name containing a path separator and confine with realpath',
      suggested_patch: '',
    },
    {
      id: 'F6', run_finding_id: 'F6', key: 'DYS-11AA22BB', recurring: false, occurrences: 1,
      title: 'Reflected XSS in search echo', severity: 'low',
      vulnerability_class: 'frontend_security_ux',
      trust_boundary: 'attacker-crafted link to victim browser',
      entry_point: 'GET /search?q=',
      sink_or_decision: 'results.html:8 {{ q | safe }} disables autoescaping',
      root_cause: 'the search term is rendered with the safe filter',
      affected_paths: ['vuln-demo/templates/results.html:8'],
      evidence: ['results.html:8 — <h2>Results for {{ q | safe }}</h2>'],
      reachability: 'reachable',
      tenant_or_instance_impact: 'session theft on a click',
      severity_rationale: 'reflected and requires a click; downgraded from medium',
      fix_recommendation: 'drop the safe filter so Jinja autoescaping applies',
      suggested_patch: '',
    },
    {
      id: 'F7', run_finding_id: 'F7', key: '', recurring: false, occurrences: 0,
      title: 'Verbose stack traces in production', severity: 'info',
      vulnerability_class: 'information_disclosure',
      trust_boundary: 'any caller to error responses',
      entry_point: 'app config',
      sink_or_decision: 'app.py:5 debug=True',
      root_cause: 'Flask debug mode enabled in the shipped config',
      affected_paths: ['vuln-demo/app.py:5'],
      evidence: [],
      reachability: 'reachable',
      tenant_or_instance_impact: 'leaks source paths and local variables on any 500',
      severity_rationale: 'informational; no direct exploit but aids other attacks',
      fix_recommendation: 'run with debug=False behind a WSGI server',
      suggested_patch: '',
    },
  ],
  rejected_candidates: [],
  gaps: [],
  class_coverage: [],
  stage_history: [],
};

const ARTEFACTS = {
  'sec-audit': [
    {
      id: 'mock-security-report',
      kind: 'security_review',
      title: 'Security harness: vuln-demo',
      bytes: 4200,
      metadata: {
        run_id: SECURITY_REPORT_DOC.run_id,
        model: SECURITY_REPORT_DOC.model.model,
        target_name: 'vuln-demo',
        report_path: 'kb/security-harness/reports/' + SECURITY_REPORT_DOC.run_id + '.json',
        findings_rollup: SECURITY_REPORT_DOC.summary,
      },
      // Markdown fallback body, shown only if the report doc fails to load.
      body: '# Security Harness Report: vuln-demo\n\n(legacy markdown fallback)\n',
    },
    {
      id: 'mock-legacy-report',
      kind: 'security_review',
      title: 'Security harness: legacy-run (no doc)',
      bytes: 900,
      // No report_path: a pre-doc artefact must keep rendering as markdown.
      metadata: { run_id: 'sec-old', model: 'claude-opus-4-7', target_name: 'legacy-run' },
      body: '# Security Harness Report: legacy-run\n\nThis pre-doc artefact renders as **markdown**.\n',
    },
    {
      id: 'mock-screenshot-result',
      kind: 'other',
      title: 'screenshot_result.txt',
      bytes: 13,
      metadata: {
        file_url: '/api/files/mock-screenshot-result',
        file_name: 'screenshot_result.txt',
        mime_type: 'text/plain; charset=utf-8',
        bytes: 172,
      },
    },
  ],
};

const REPORT_DOCS = {
  ['kb/security-harness/reports/' + SECURITY_REPORT_DOC.run_id + '.json']: SECURITY_REPORT_DOC,
};

const FILES = {
  'mock-screenshot-result': [
    'Page loaded',
    '',
    'Title: TargetPractice - Sharpen your hacking skills',
    'URL: https://targetpractice.network/',
    'Status: Loaded and rendered successfully',
    '',
    'Screenshot taken.',
  ].join('\n'),
};

const MIND_FILES = {
  'AGENTS.md': [
    '# AGENTS.md - Operating Procedures',
    '',
    '## Every Session',
    '',
    'Before doing anything else:',
    '1. Read SOUL.md - this is who you are',
    '2. Read IDENTITY.md - this is your context',
    "3. Read today's journal for recent context",
    '4. Read MEMORY.md for long-term context',
  ].join('\n'),
  'IDENTITY.md': '# IDENTITY.md\\n\\nName: Dyson\\n',
  'HEARTBEAT.md': '# HEARTBEAT.md\\n\\nStill here.\\n',
  'MEMORY.md': '# MEMORY.md\\n\\nUseful operator context lives here.\\n',
  'USER.md': '# USER.md\\n\\nThe operator likes direct answers.\\n',
  'memory/projects.md': '# Projects\\n\\n- Dyson UI polish\\n',
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
        if (url === '/api/agent') return json(res, { name: 'Dyson' });
        if (url === '/api/commands') return json(res, []);
        if (url === '/api/mcp/elicitations') return json(res, []);
        if (url === '/api/mcp/servers') return json(res, { servers: [] });
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
        const mindFileMatch = url.match(/^\/api\/mind\/file\?path=(.+)$/);
        if (mindFileMatch && req.method === 'GET') {
          const filePath = decodeURIComponent(mindFileMatch[1]);
          // Report docs are the same JSON-in-content envelope the mind route
          // serves for any workspace file — this is the path SecurityReportView
          // fetches and JSON.parses.
          if (filePath in REPORT_DOCS) {
            return json(res, { path: filePath, content: JSON.stringify(REPORT_DOCS[filePath]) });
          }
          return json(res, { path: filePath, content: MIND_FILES[filePath] || '' });
        }
        if (url === '/api/mind/file' && req.method === 'POST') {
          const body = await readBody(req);
          if (body.path) MIND_FILES[body.path] = body.content || '';
          return json(res, { ok: true });
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
        if (artListMatch) {
          const id = decodeURIComponent(artListMatch[1]);
          return json(res, ARTEFACTS[id] || []);
        }
        const scopedArtMatch = url.match(/^\/api\/conversations\/([^/?]+)\/artefacts\/([^/?]+)$/);
        if (scopedArtMatch && req.method === 'GET') {
          const chat = decodeURIComponent(scopedArtMatch[1]);
          const id = decodeURIComponent(scopedArtMatch[2]);
          const art = (ARTEFACTS[chat] || []).find(a => a.id === id);
          if (!art) return json(res, { error: 'not found' }, 404);
          res.statusCode = 200;
          res.setHeader('content-type', 'text/plain; charset=utf-8');
          res.setHeader('X-Dyson-Chat-Id', chat);
          res.end(art.body || art.metadata?.file_url || '');
          return;
        }
        const globalArtMatch = url.match(/^\/api\/artefacts\/([^/?]+)$/);
        if (globalArtMatch && req.method === 'GET') {
          const id = decodeURIComponent(globalArtMatch[1]);
          for (const [chat, list] of Object.entries(ARTEFACTS)) {
            const art = list.find(a => a.id === id);
            if (!art) continue;
            res.statusCode = 200;
            res.setHeader('content-type', 'text/plain; charset=utf-8');
            res.setHeader('X-Dyson-Chat-Id', chat);
            res.end(art.metadata?.file_url || '');
            return;
          }
          return json(res, { error: 'not found' }, 404);
        }
        const fileMatch = url.match(/^\/api\/files\/([^/?]+)$/);
        if (fileMatch && req.method === 'GET') {
          const id = decodeURIComponent(fileMatch[1]);
          if (!(id in FILES)) return json(res, { error: 'not found' }, 404);
          res.statusCode = 200;
          res.setHeader('content-type', 'text/plain; charset=utf-8');
          res.end(FILES[id]);
          return;
        }
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
