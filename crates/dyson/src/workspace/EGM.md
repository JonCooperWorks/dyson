# Workspace (`workspace`)

## Overview
The agent's persistent state: identity (`SOUL.md`, `IDENTITY.md`), memory (`MEMORY.md`), daily journals (`memory/YYYY-MM-DD.md`), skills, and knowledge-base files. The `Workspace` trait abstracts storage; `FilesystemWorkspace` stores Markdown files on disk with an SQLite FTS5 index for overflow memory, `ChannelWorkspace` is a write-restricted wrapper for public channel agents, and `migrate` upgrades on-disk layouts.

## Interface
- `Workspace` trait: `get`, `set`, `append`, `remove`, `save`, `list_files`, `search`, `system_prompt`, `journal`, `char_limit`, `char_ceiling`, `nudge_interval`, `memory_search`, `skill_dirs`, `programs_dir`, `set_attribution` [mod.rs:Workspace]
- `WorkspaceHandle = Arc<RwLock<Box<dyn Workspace>>>` [mod.rs:WorkspaceHandle]
- `create_workspace(config)` and `create_channel_workspace(config, channel_id)` [mod.rs:create_workspace, mod.rs:create_channel_workspace]
- `FilesystemWorkspace` - `load`, `load_default`, `load_from_connection_string`, `resolve_path`, `path`, `today_journal`, `yesterday_journal` [filesystem.rs:FilesystemWorkspace]
- `InMemoryWorkspace` - non-persistent, for tests [in_memory.rs:InMemoryWorkspace]
- `ChannelWorkspace` [channel.rs:ChannelWorkspace]
- `MemoryStore` (`open`, `index`, search) and `SearchResult` [memory_store.rs:MemoryStore, memory_store.rs:SearchResult]
- `migrate::{CURRENT_WORKSPACE_VERSION = 3, read_version, migrate}` [migrate.rs:CURRENT_WORKSPACE_VERSION, migrate.rs:migrate]

## Invariants
- Only backend `"filesystem"` is supported [mod.rs:create_workspace]
- Workspace files over 10 MB are skipped on load [filesystem.rs:FilesystemWorkspace::load]; KB file loads capped at 10 MiB [filesystem.rs:KB_LOAD_FILE_MAX_BYTES]
- `FilesystemWorkspace::load` runs migrations before reading; the version lives in `.workspace_version` (0 if missing) [migrate.rs:migrate, migrate.rs:VERSION_FILE]
- `ChannelWorkspace` is a whitelist: writes to keys outside the writable set are silently dropped, including `_audit.jsonl` [channel.rs:ChannelWorkspace, channel.rs:AUDIT_LOG_KEY]
- Channel journals older than `DEFAULT_MAX_JOURNAL_AGE_DAYS = 90` are pruned [channel.rs:DEFAULT_MAX_JOURNAL_AGE_DAYS]

## Side Effects
- Reads and writes Markdown files under the workspace directory [filesystem.rs:FilesystemWorkspace]
- Opens or creates SQLite `memory.db` with an FTS5 table `memory_fts(key, content)` [filesystem.rs:FilesystemWorkspace::load, memory_store.rs:MemoryStore::open]
- Migrations rename/move files and rewrite `.workspace_version` [migrate.rs:migrate]
- Channel workspaces create `channels/<id>/` and symlink `SOUL.md` / `IDENTITY.md` from the main workspace; writes are attributed in `_audit.jsonl` [mod.rs:create_channel_workspace, channel.rs:ChannelWorkspace]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
