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
├── MEMORY.md            curated long-term memory  (Tier 1, 2500 soft / 3375 ceiling)
├── USER.md              user profile              (Tier 1, 1375 char limit)
├── AGENTS.md            operating procedures
├── HEARTBEAT.md         periodic task checklist (reserved for future use)
├── memory/
│   ├── 2026-03-20.md    today's journal           (Tier 3)
│   ├── 2026-03-19.md    yesterday's journal       (Tier 3)
│   └── notes/           overflow storage           (Tier 2)
│       └── *.md
├── kb/
│   ├── INDEX.md         navigation index         (in system prompt)
│   ├── raw/             source material           (FTS5 indexed)
│   └── wiki/            curated articles           (FTS5 indexed)
├── skills/              local skills (auto-discovered)
│   └── <name>/SKILL.md  skill body plus optional frontmatter
├── programs/            agent working directory for code/shell tools
└── memory.db            SQLite FTS5 index
```

Default files are created automatically when the workspace is first loaded
(`src/workspace/filesystem.rs`).

---

## Workspace Versioning and Migration

Workspaces are versioned via `.workspace_version` (missing = version 0). Migrations run automatically on every load — existing workspaces upgrade in place on next startup.

| Version | Description |
|---------|-------------|
| 0 → 1 | Create `memory/notes/` directory for Tier 2 overflow |
| 1 → 2 | Promote legacy `skills/*.md` files to `skills/<name>/SKILL.md` |
| 2 → 3 | Create `kb/` directory structure for knowledge base |

To add a migration: bump `CURRENT_WORKSPACE_VERSION` in `src/workspace/migrate.rs` and add a `Migration` to `migrations()`.

`dyson init` auto-detects existing filesystem workspaces (presence of `SOUL.md` + `IDENTITY.md`) and migrates in place, or use `--import-filesystem <path>` to import from another directory.

---

## Character Limits — Fuzzy Soft Target + Hard Ceiling

`MEMORY.md` and `USER.md` are not capped by a single hard limit. Each file has
a **soft target** (what the curator aims for) and a **hard ceiling** (the only
size the tool actually refuses). Writes in the overflow band between the two
succeed with a warning — 2,700 chars of valuable signal is better than 2,470
chars of truncated context.

| File | Soft Target | Hard Ceiling (target × 1.35) |
|------|-------------|------------------------------|
| `MEMORY.md` | 2,500 chars | 3,375 chars |
| `USER.md` | 1,375 chars | 1,856 chars |

`workspace` op=update behaviour:

- **At or below soft target** — success, `[current/target chars]`.
- **In the overflow band** — success, `[current/target chars — over soft target, within ceiling N]`.
- **Above the ceiling** — rejected with a "Would exceed hard ceiling" error.

The ceiling is derived from `soft_target × overflow_factor`. Tune the factor
in `dyson.json` under `workspace.memory.overflow_factor` (default `1.35`).
Other files (`SOUL.md`, `AGENTS.md`) have no limit.

Curation — the process of picking what to keep — is handled by the
`LearningSynthesisDream` and `MemoryMaintenanceDream` (see `docs/dreaming.md`).
Both apply a Keep / Refine / Discard judgment that deliberately ignores
timestamps so night sessions are never penalised.

---

## FTS5 Full-Text Search

`MemoryStore` wraps a SQLite FTS5 virtual table indexed by file key and content. All `memory/` files are indexed on load; writes auto-update the index.

The `memory_search` tool queries FTS5 (top 20 results with highlighted snippets), falling back to case-insensitive regex search if no FTS5 matches.

---

## Memory Nudges

Every N turns (default: 7), the agent loop injects a maintenance nudge reporting character usage and suggesting the agent save important details. Set `nudge_interval: 0` to disable.

---

## System Prompt Composition

Assembled from: SOUL.md → IDENTITY.md → MEMORY.md → USER.md → yesterday's journal → today's journal. Empty files omitted, sections separated by `---`.

---

## Configuration

Memory settings live in `dyson.json` under `workspace.memory`:

```json
{
  "workspace": {
    "backend": "filesystem",
    "connection_string": "~/.dyson",
    "memory": {
      "limits": {
        "MEMORY.md": 2500,
        "USER.md": 1375
      },
      "overflow_factor": 1.35,
      "nudge_interval": 7
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `limits` | `{string: number}` | See above | Per-file **soft** character targets. Files not listed have no limit. |
| `overflow_factor` | `number` | `1.35` | Multiplier that turns a soft target into a hard ceiling. Writes between the two succeed with a warning. |
| `nudge_interval` | `number` | `7` | Inject nudge every N turns. `0` disables nudges. |

---

## Context Compaction

Dyson automatically compresses conversation history when the estimated token
count approaches the model's context window.  This is separate from memory
file limits — it handles the **conversation** buffer, not the persistent
workspace files.

Configuration lives in `dyson.json` under `agent.compaction`:

```json
{
  "agent": {
    "compaction": {
      "context_window": 200000,
      "threshold_ratio": 0.50,
      "protect_head": 3,
      "protect_tail_tokens": 20000,
      "summary_min_tokens": 2000,
      "summary_max_tokens": 12000,
      "summary_target_ratio": 0.20
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `context_window` | `number` | `200000` | Model's context window in estimated tokens. |
| `threshold_ratio` | `number` | `0.50` | Trigger compaction at this fraction of context_window. |
| `protect_head` | `number` | `3` | Always keep the first N messages (never summarised). |
| `protect_tail_tokens` | `number` | `20000` | Keep recent messages within this token budget. |
| `summary_min_tokens` | `number` | `2000` | Minimum summary output tokens. |
| `summary_max_tokens` | `number` | `12000` | Maximum summary output tokens. |
| `summary_target_ratio` | `number` | `0.20` | Summary size as a fraction of the middle section. |

Shorthand: `"compaction": 200000` sets `context_window` with all other fields
defaulting. Omit the key to use the built-in defaults. There is currently no
boolean disable flag; set a very high `context_window` or `threshold_ratio` if
you need to avoid automatic compaction.

The algorithm uses five phases: tool output pruning, region identification,
structured LLM summarisation (Goal / Progress / Decisions / Files / Next Steps),
reassembly, and orphaned tool pair repair.  See
[comparison-hermes-agent.md](comparison-hermes-agent.md) for details.

---

## Key Source Files

| Component | File |
|-----------|------|
| MemoryStore (FTS5) | `src/workspace/memory_store.rs` |
| FilesystemWorkspace | `src/workspace/filesystem.rs` |
| Workspace trait | `src/workspace/mod.rs` |
| Workspace migration | `src/workspace/migrate.rs` |
| MemoryConfig | `src/config/mod.rs` |
| Nudge injection | `src/agent/mod.rs` |
| memory_search tool | `src/tool/memory_search.rs` |
| KbSearchTool | `src/tool/kb_search.rs` |
| KbStatusTool | `src/tool/kb_status.rs` |
| workspace tool | `src/tool/workspace.rs` |

---

## Acknowledgements

The tiered memory architecture — agent-curated files with character limits,
FTS5 search for overflow, periodic nudges, and daily journals — is inspired
by [Hermes Agent](https://github.com/nousresearch/hermes-agent) by
[Nous Research](https://nousresearch.com/).  Dyson's workspace file format
is also compatible with the filesystem/TARS format used by Hermes.

---

See also: [Knowledge Base](knowledge-base.md) ·
[Architecture Overview](architecture-overview.md) ·
[Tools & Skills](tools-and-skills.md) · [Configuration](configuration.md)
