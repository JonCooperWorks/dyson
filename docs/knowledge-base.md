# Knowledge Base

The knowledge base (KB) is a structured document storage and full-text search
system that gives the agent a persistent, searchable library of reference
material.  Raw sources go into `kb/raw/`, curated articles into `kb/wiki/`,
and an optional `kb/INDEX.md` provides a navigation index that is included
directly in the system prompt.

Inspired by Andrej Karpathy's concept of agents maintaining a personal
wiki/knowledge base
([tweet](https://x.com/karpathy/status/2039805659525644595?s=46)).

**Key files:**

- `src/tool/kb_search.rs` — `KbSearchTool` (FTS5 search with scope filtering)
- `src/tool/kb_status.rs` — `KbStatusTool` (file counts, sizes, directory listing)
- `src/workspace/filesystem.rs` — KB loading, `INDEX.md` injection into system prompt
- `src/workspace/memory_store.rs` — FTS5 indexing (`memory_fts` virtual table)
- `src/workspace/migrate.rs` — v2 → v3 migration (creates `kb/` structure)

---

## Directory Layout

```
~/.dyson/
├── ...
├── kb/
│   ├── INDEX.md             navigation index (included in system prompt)
│   ├── raw/                 raw source material
│   │   ├── paper.md
│   │   └── meeting-notes.md
│   └── wiki/                curated articles (agent-maintained)
│       ├── rust/
│       │   └── ownership.md
│       └── project-setup.md
├── memory.db                SQLite FTS5 index (shared with memory system)
└── ...
```

| Path | Purpose |
|------|---------|
| `kb/raw/` | Raw source material — drop articles, papers, meeting notes here |
| `kb/wiki/` | Curated articles — agent-maintained on request, organized by topic |
| `kb/INDEX.md` | Optional navigation index; when present, included in system prompt under `## KNOWLEDGE BASE` |

Subdirectories are fully supported — files are loaded recursively from both
`raw/` and `wiki/`.

---

## How It Works

```
┌──────────────┐     ┌────────────────┐     ┌──────────────┐
│  Workspace   │────>│  Recursive     │────>│  FTS5 Index   │
│  load()      │     │  file scan     │     │  (memory.db)  │
│              │     │  kb/**/*.md    │     │               │
└──────────────┘     └────────────────┘     └───────┬───────┘
                                                    │
       ┌────────────────────────────────────────────┤
       │                                            │
       v                                            v
┌──────────────┐                          ┌─────────────────┐
│  INDEX.md    │                          │  kb_search tool  │
│  → system    │                          │  FTS5 query      │
│    prompt    │                          │  → top 20 hits   │
└──────────────┘                          └─────────────────┘
```

1. On startup, `FilesystemWorkspace::load()` recursively scans `kb/raw/` and
   `kb/wiki/`, loading all `.md` files into the workspace.
2. All loaded KB files are indexed into the `memory_fts` FTS5 virtual table
   (shared with the memory system).
3. If `kb/INDEX.md` exists and is non-empty, its content is injected into the
   system prompt under `## KNOWLEDGE BASE`.
4. At runtime the agent queries the KB via the `kb_search` tool.  Writes via
   the `workspace` tool (op=update) automatically re-index the affected file.

---

## Tools

### kb_search

Search the knowledge base using full-text search.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `query` | string | yes | Words or phrases to find in the knowledge base |
| `scope` | string | no | `"all"` (default), `"raw"` (source material only), or `"wiki"` (curated articles only) |

Behaviour:

- Queries the shared `memory_fts` FTS5 table, then filters results to keys
  matching the scope prefix (`kb/`, `kb/raw/`, or `kb/wiki/`).
- Returns up to 20 results with 64-character highlighted snippets (FTS5
  `snippet()` with `**` markers).
- Falls back to case-insensitive regex search if FTS5 returns no matches.

### kb_status

Show knowledge base statistics.  Takes no parameters.

Reports:

- Raw source file count and total size
- Wiki article file count and total size
- Whether `INDEX.md` is present
- Lists all files in each section

Useful for understanding what's in the KB before searching or adding content.

---

## Workspace Migration

The `kb/` directory structure is created automatically by the v2 → v3
workspace migration.  Existing workspaces upgrade on next startup.

| Version | Migration |
|---------|-----------|
| 2 → 3 | Create `kb/`, `kb/raw/`, `kb/wiki/` |

New workspaces get the directories directly from `load()` (via
`create_dir_all`).

---

## FTS5 Integration

KB files share the same `memory_fts` SQLite FTS5 virtual table used by the
memory system:

```sql
CREATE VIRTUAL TABLE memory_fts USING fts5(key, content)
```

KB files are distinguished by their key prefix (`kb/raw/...`, `kb/wiki/...`).
The `kb_search` tool filters on this prefix; the `memory_search` tool filters
on `memory/`.  Both use the same underlying `MemoryStore::search()` method.

| Detail | Value |
|--------|-------|
| Ranking | FTS5 `rank` (BM25 relevance) |
| Snippets | 64-char window, `**` highlight markers, `...` ellipsis |
| Result limit | 20 per query |
| Fallback | Case-insensitive regex over workspace files |

---

## Usage Patterns

1. **Seeding the KB** — Drop `.md` files into `kb/raw/` on disk, then restart
   the agent.  Files are indexed automatically on load.

2. **Agent-maintained wiki** — Ask the agent to read raw sources and compile
   them into wiki articles under `kb/wiki/`.  The agent writes via the
   `workspace` tool (op=update).

3. **INDEX.md curation** — Create `kb/INDEX.md` with a topic map.  This goes
   into the system prompt so the agent always knows what's available without
   searching first.

4. **Scoped search** — Use `scope: "wiki"` to search only curated content, or
   `scope: "raw"` for source material only.

---

## Key Source Files

| Component | File |
|-----------|------|
| KbSearchTool | `src/tool/kb_search.rs` |
| KbStatusTool | `src/tool/kb_status.rs` |
| KB loading & INDEX.md | `src/workspace/filesystem.rs` |
| FTS5 index | `src/workspace/memory_store.rs` |
| v2 → v3 migration | `src/workspace/migrate.rs` |
| Workspace trait | `src/workspace/mod.rs` |

---

## Acknowledgements

The knowledge base concept — an agent maintaining a personal wiki of
structured reference material — is inspired by
[Andrej Karpathy](https://x.com/karpathy/status/2039805659525644595?s=46).

---

See also: [Memory](memory.md) ·
[Tools & Skills](tools-and-skills.md) · [Architecture Overview](architecture-overview.md)
