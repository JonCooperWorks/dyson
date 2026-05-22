/* Cold-load fallback slash commands.  The live app replaces this with
 * /api/commands so executable local skills can show up without a frontend
 * deploy. */

export const FALLBACK_SLASH_COMMANDS = Object.freeze([
  { cmd: '/clear',     desc: 'Clear this conversation (keep history searchable)', src: 'controller' },
  { cmd: '/compact',   desc: 'Summarise transcript in-place to free context',     src: 'controller' },
  { cmd: '/model',     desc: 'Switch model for this conversation',                src: 'controller' },
  { cmd: '/loop',      desc: 'Schedule a recurring prompt',                       src: 'controller' },
  { cmd: '/stop',      desc: 'Cancel the current turn',                           src: 'controller' },
  { cmd: '/agents',    desc: 'List running background agents',                    src: 'controller' },
  { cmd: '/fork-from', desc: 'Fork a new conversation from a point',              src: 'web' },
  // `/export` used to live here as a slash command but the server-side
  // tool writes to the workspace, and on the web deployment that path
  // doesn't resolve the same way Telegram's does — replaced with a
  // download button in the transcript header that hits
  // GET /api/conversations/<id>/export directly.
].map(Object.freeze));

export const SLASH_COMMANDS = FALLBACK_SLASH_COMMANDS;
