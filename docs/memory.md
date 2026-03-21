# Memory System

Dyson uses a tiered memory architecture inspired by
[Hermes Agent](https://github.com/nousresearch/hermes-agent) from
[Nous Research](https://nousresearch.com/).  The design follows the same
principles — agent-curated memory with periodic nudges, persistent identity
files, and full-text search for overflow — adapted for Dyson's Rust-native
workspace system.

---

## Three-Tier Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Tier 1 — Always-in-Context                             │
│  MEMORY.md (2,200 chars)  ·  USER.md (1,375 chars)     │
│  Included in every system prompt                        │
├─────────────────────────────────────────────────────────┤
│  Tier 2 — Searchable Archive                            │
│  memory/notes/*.md                                      │
│  Indexed by SQLite FTS5, queried via memory_search      │
├─────────────────────────────────────────────────────────┤
│  Tier 3 — Historical Journals                           │
│  memory/YYYY-MM-DD.md  (one per day)                    │
│  Yesterday + today auto-included in system prompt       │
└─────────────────────────────────────────────────────────┘
```

**Tier 1** files are always injected into the system prompt.  They have
enforced character limits to keep context usage predictable.

**Tier 2** files live in `memory/notes/` and are indexed in a SQLite FTS5
virtual table.  The agent queries them with the `memory_search` tool when it
needs to recall something that doesn't fit in Tier 1.

**Tier 3** journals are daily log files (`memory/YYYY-MM-DD.md`).  The agent
appends entries throughout the day.  Yesterday's and today's journals are
included in the system prompt for session continuity.

---

## Workspace File Layout

```
~/.dyson/
├── SOUL.md              personality, vibe, behavioral guidelines
├── IDENTITY.md          who the agent is, capabilities
├── MEMORY.md            curated long-term memory  (Tier 1, 2200 char limit)
├── USER.md              user profile              (Tier 1, 1375 char limit)
├── AGENTS.md            operating procedures
├── HEARTBEAT.md         periodic task checklist (reserved for future use)
├── memory/
│   ├── 2026-03-20.md    today's journal           (Tier 3)
│   ├── 2026-03-19.md    yesterday's journal       (Tier 3)
│   └── notes/           overflow storage           (Tier 2)
│       └── *.md
├── skills/              local skill files (auto-discovered)
│   └── *.md             SKILL.md format with frontmatter
└── memory.db            SQLite FTS5 index
```

Default files are created automatically when the workspace is first loaded
(`src/workspace/openclaw.rs`).

---

## Workspace Versioning and Migration

Workspaces are versioned, following the same pattern as
[config migration](configuration.md).  The version is stored in a
`.workspace_version` file at the workspace root.  A missing file means
version 0 (bare OpenClaw/TARS format).

### How it works

1. `OpenClawWorkspace::load()` calls `migrate::migrate(path)` before
   reading files.
2. `migrate()` reads `.workspace_version` (defaults to 0 if missing).
3. It runs each migration's steps in order, then stamps the new version.
4. `ensure_defaults()` runs after migration to create any missing content
   files (USER.md, HEARTBEAT.md, etc.).

Migrations are **automatic** — they run on every workspace load, not just
during `dyson init`.  If you have an existing workspace, the next startup
will upgrade it in place.

### Migration chain

| Version | Description |
|---------|-------------|
| 0 → 1 | Create `memory/notes/` directory for Tier 2 overflow |

### Step operations

Migrations use a declarative `Step` enum (same philosophy as config
migration, but for filesystem operations):

| Step | Description |
|------|-------------|
| `CreateDir(path)` | Create a directory (and parents). No-op if it exists. |
| `Rename(from, to)` | Rename/move a file. No-op if source missing. |
| `SkipIf(path)` | Skip remaining steps if path exists. |
| `BailIf(path, msg)` | Error if path exists (ambiguous state). |

### Adding a new migration

1. Bump `CURRENT_WORKSPACE_VERSION` in `src/workspace/migrate.rs`.
2. Add a `Migration` to the `migrations()` function.
3. That's it — the chain handles the rest.

### OpenClaw import on init

`dyson init` auto-detects existing OpenClaw workspaces.  If the workspace
directory already contains `SOUL.md` and `IDENTITY.md`, it is recognized as
an OpenClaw workspace and migrated in place — no flags needed.

You can also explicitly import from a different directory:

```sh
dyson init --noinput --import_openclaw /path/to/openclaw/workspace
```

This copies `.md` files into the Dyson workspace directory, then the
migration chain and `ensure_defaults()` bring them up to current format.

---

## Character Limits and Enforcement

Tier 1 files have per-file character limits defined in `MemoryConfig`:

| File | Default Limit |
|------|--------------|
| `MEMORY.md` | 2,200 chars |
| `USER.md` | 1,375 chars |

When the `workspace_update` tool processes a write, it:

1. Checks the would-be length (for both `set` and `append` modes).
2. **Rejects** the write if it would exceed the limit.
3. Returns an error with current usage stats and a suggestion to move
   overflow to `memory/notes/`.

Successful writes report usage as `[current/limit chars]` in the tool
response so the agent always knows how much room is left.

Files without a configured limit (e.g. `SOUL.md`, `AGENTS.md`) accept
unlimited content.

---

## FTS5 Full-Text Search

`MemoryStore` (`src/workspace/memory_store.rs`) wraps a SQLite database with
a single FTS5 virtual table:

```sql
CREATE VIRTUAL TABLE memory_fts USING fts5(key, content)
```

- **key** — workspace file name (e.g. `memory/notes/rust.md`)
- **content** — the full file text

### Indexing

Every `set()` or `append()` call on a `memory/` file automatically updates
the FTS5 index (delete + insert upsert pattern).  All existing `memory/`
files are indexed on workspace load.

### Searching

The `memory_search` tool accepts a query string.  Internally:

1. Each word is wrapped in quotes for FTS5 safety (prevents syntax errors).
2. FTS5 returns the top 20 results ranked by relevance.
3. Snippets are highlighted with `**bold**` markers (64-char window).
4. If FTS5 returns no results, the system falls back to case-insensitive
   regex search over `memory/` files.

### `memory_search` tool

```json
{
  "name": "memory_search",
  "input": { "query": "rust error handling" }
}
```

Returns matching file paths and highlighted snippet excerpts.

---

## Memory Nudges

The agent loop injects a maintenance nudge into the conversation every N
turns (default: 5).  The nudge is a system-injected user message that:

- Reports current character usage for `MEMORY.md` and `USER.md`
- Suggests saving important details from the conversation
- Points to `workspace_update` and `memory_search` tools
- Recommends moving overflow to `memory/notes/`

Format:

```
[System: Memory Maintenance] Consider saving important details from this conversation.
MEMORY.md: 1200/2200 chars. USER.md: 800/1375 chars.
Use workspace_view/workspace_update. Move overflow to memory/notes/ (searchable via memory_search).
```

The nudge interval can be set to 0 to disable nudges entirely.

---

## System Prompt Composition

`system_prompt()` (`src/workspace/openclaw.rs`) assembles the prompt from
Tier 1 files and journals, separated by `---`:

1. **PERSONALITY** — `SOUL.md`
2. **IDENTITY** — `IDENTITY.md`
3. **LONG-TERM MEMORY** — `MEMORY.md`
4. **USER PROFILE** — `USER.md`
5. **YESTERDAY'S JOURNAL** — `memory/YYYY-MM-DD.md` (if exists)
6. **TODAY'S JOURNAL** — `memory/YYYY-MM-DD.md` (if exists)

Empty files are omitted.

---

## Configuration

Memory settings live in `dyson.json` under `workspace.memory`:

```json
{
  "workspace": {
    "backend": "openclaw",
    "connection_string": "~/.dyson",
    "memory": {
      "limits": {
        "MEMORY.md": 2200,
        "USER.md": 1375
      },
      "nudge_interval": 5
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `limits` | `{string: number}` | See above | Per-file character limits. Files not listed have no limit. |
| `nudge_interval` | `number` | `5` | Inject nudge every N turns. `0` disables nudges. |

---

## Key Source Files

| Component | File |
|-----------|------|
| MemoryStore (FTS5) | `src/workspace/memory_store.rs` |
| OpenClawWorkspace | `src/workspace/openclaw.rs` |
| Workspace trait | `src/workspace/mod.rs` |
| Workspace migration | `src/workspace/migrate.rs` |
| MemoryConfig | `src/config/mod.rs` |
| Nudge injection | `src/agent/mod.rs` |
| memory_search tool | `src/tool/memory_search.rs` |
| workspace_update tool | `src/tool/workspace_update.rs` |

---

## Acknowledgements

The tiered memory architecture — agent-curated files with character limits,
FTS5 search for overflow, periodic nudges, and daily journals — is inspired
by [Hermes Agent](https://github.com/nousresearch/hermes-agent) by
[Nous Research](https://nousresearch.com/).  Dyson's workspace file format
is also compatible with the OpenClaw/TARS format used by Hermes.

---

See also: [Architecture Overview](architecture-overview.md) ·
[Tools & Skills](tools-and-skills.md) · [Configuration](configuration.md)
