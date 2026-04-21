// Dyson — minimal client-side state shell.
//
// All real values are injected at runtime by js/bridge.js from the
// HttpController.  This file declares ONLY the structure with empty
// values and the slash-command catalogue (which is a real list of
// commands the controller honours, not synthesised data).
//
// Nothing in here may be invented numbers, fake providers, or example
// conversations.  Live mode populates everything from /api/*.
window.DYSON_DATA = {
  // Active model name — bridge fills from /api/providers.
  activeModel: '',

  // Conversations grouped by controller.  Bridge fills `http` from
  // /api/conversations.  Telegram and Swarm groups remain empty unless
  // a future endpoint surfaces those controllers' chat lists too.
  conversations: { http: [], telegram: [], swarm: [] },

  // Per-conversation transcripts and per-tool panel data are populated
  // on demand by the live conversation view.
  convo: [],
  tools: {},
  subagents: {},

  // Providers — bridge fills from /api/providers.
  providers: [],

  // Background activity (loops, dreams, swarm tasks) — empty until the
  // controller aggregates cross-controller registries.
  activity: [],
  checkpoints: [],

  // Mind / workspace — bridge fills from /api/mind + /api/mind/file.
  mind: {
    backend: '',
    heartbeat: '',
    files: [],
    open: { path: '', content: '', recentEdits: [] },
  },

  // Sandbox / skills — bridge fills builtin from /api/skills.  Policy
  // and call counts are not currently exposed by the controller.
  skills: { builtin: [], mcp: [], denials: [] },

  // Slash commands the controller actually understands.  Real list,
  // matched in the composer's slash menu.
  slashCmds: [
    { cmd: '/clear',     desc: 'Clear this conversation (keep history searchable)', src: 'controller' },
    { cmd: '/compact',   desc: 'Summarise transcript in-place to free context',     src: 'controller' },
    { cmd: '/model',     desc: 'Switch model for this conversation',                src: 'controller' },
    { cmd: '/loop',      desc: 'Schedule a recurring prompt',                       src: 'controller' },
    { cmd: '/stop',      desc: 'Cancel the current turn',                           src: 'controller' },
    { cmd: '/agents',    desc: 'List running background agents',                    src: 'controller' },
    { cmd: '/fork-from', desc: 'Fork a new conversation from a point',              src: 'web' },
    { cmd: '/export',    desc: 'Export transcript (md, json)',                      src: 'web' },
  ],
};
