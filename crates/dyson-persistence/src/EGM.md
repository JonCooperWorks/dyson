# dyson-persistence src

## Overview
This crate stores durable per-chat conversation history and the per-chat execution journal. It defines the `ChatHistory` trait, a disk backend (`DiskChatHistory`) that keeps one directory per chat, a coalescing background checkpointer, and a versioned migration for the chats directory layout ([lib.rs:module comment], [disk.rs:module comment]).

## Interface
- Modules and re-exports: `pub mod disk`, `pub mod migrate`, `pub use coalesce::CoalescingPersister`, `pub use disk::DiskChatHistory` ([lib.rs]).
- `trait ChatHistory: Send + Sync` ([lib.rs:ChatHistory]):
  - Required methods: `save(chat_id, &[Message])`, `load(chat_id) -> Vec<Message>` and `rotate(chat_id)`.
  - Defaulted transcript methods: `save_title`, `load_title`, `remove_title`, `remove` and `list`.
  - Defaulted harness-record methods: `list_harness_chats`, `save_harness_record` and `load_harness_record`.
  - Defaulted run-journal methods: `append_run_event`, `append_run_events`, `load_run_events` and `load_unresolved_tool_outcomes`.
- `DiskChatHistory::new(dir: PathBuf)` and `DiskChatHistory::new_from_connection_string(&str)`, which expands `~` using `$HOME` ([disk.rs:DiskChatHistory], [lib.rs:resolve_tilde]).
- `CoalescingPersister` ([coalesce.rs:CoalescingPersister]):
  - `new(history, chat_id)`.
  - `schedule(Vec<Message>)`: non-blocking, latest snapshot wins.
  - `async flush()`.
  - `async checkpoint(Vec<Message>)`: schedule plus flush.
- `migrate::migrate(dir) -> Result<bool>` and `migrate::CURRENT_CHATS_VERSION` (1) ([migrate.rs:migrate]).

## Invariants
- On-disk layout under `{dir}/{chat_id}/` ([disk.rs]):
  - `transcript.json`
  - `title.txt`
  - `archives/{YYYY-MM-DDTHH-MM-SS}.json`
  - `media/{hash}.b64`
  - `harness/{key}.json`
  - `run-events.jsonl`
- `save` writes atomically. It writes a hidden temp file `.{name}.tmp.{pid}.{n}`, then `sync_all`, then renames it over the transcript. On Unix the replacement keeps the target's mode bits (default `0o600`) ([disk.rs:write_atomically], [disk.rs:create_replacement_file]).
- `save` moves inline `Image` and `Document` base64 data into `media/{hash}.b64` and leaves `@media/{hash}` references in the transcript. `load` restores them, and a failed restore keeps the reference ([disk.rs:externalize_images], [disk.rs:restore_images], [disk.rs:MEDIA_REF_PREFIX]).
- Harness record `chat_id` and `key` values must be non-empty `[A-Za-z0-9_-]`. Otherwise the call fails with "invalid harness record identifier". Records are written to a temp file with mode `0o600`, fsynced, renamed, and then the directory is fsynced ([disk.rs:save_harness_record], [disk.rs:load_harness_record]).
- Run-journal appends ([disk.rs:append_run_events]):
  - An append holds one of 64 hashed per-chat mutexes.
  - A torn final JSONL record (no trailing newline) is truncated before appending.
  - Each append ends with `sync_data`. The parent directory is also fsynced the first time the journal is created.
  - The cached outcome projection is updated only after the write is durable, and only if the file stamp (len, mtime, dev/ino) has not changed.
- `load_run_events` ignores only an unterminated final malformed line. Any earlier malformed record is a hard error ([disk.rs:load_run_events]).
- The unresolved-outcome cache is keyed by journal stamp and cleared once it reaches 128 chats ([disk.rs:load_unresolved_tool_outcomes]).
- `list` returns chats that have `transcript.json`, newest mtime first, with ties broken by chat id in reverse order ([disk.rs:list]).
- `rotate` renames the transcript into `archives/` instead of deleting it. `remove` runs `remove_dir_all` on the whole chat directory ([disk.rs:rotate], [disk.rs:remove]).
- Trait defaults: `save_harness_record` returns `Err` ("history backend does not support durable task records"), and the journal methods are no-ops or empty ([lib.rs:ChatHistory]).
- `CoalescingPersister` ([coalesce.rs]):
  - Debounces for 100 ms (`DEBOUNCE`) and writes only the newest snapshot.
  - Marks a generation complete even when the save fails, so `flush` cannot hang.
  - The worker exits when the last handle is dropped.
- `migrate` ([migrate.rs:migrate]):
  - A version above `CURRENT_CHATS_VERSION` is a `Config` error. A gap in the migration chain is also a `Config` error.
  - A directory without `.chats_version` starts at v0 only if it contains flat-layout files. Otherwise it starts at the current version.
  - The v0 to v1 migration steps are `FlattenToPerChatDirs` and `FanOutSharedArtefacts`, and both are idempotent ([migrate.rs:migrations]).
- `DiskChatHistory::new` logs a migration failure instead of failing construction ([disk.rs:DiskChatHistory::new]).

## Side Effects
- Disk: creates, writes, renames and removes files and directories under the chats root, and calls fsync on files and directories ([disk.rs]).
- Disk (migration): `.chats_version` writes, plus file moves that also relocate shared `artefacts/*.meta.json` entries by their `chat_id` ([migrate.rs:write_version], [migrate.rs:fan_out_shared_artefacts]).
- Locks: 64 striped `std::sync::Mutex<()>` journal locks, the `outcomes` cache mutex, and the `CoalescingPersister` snapshot mutex ([disk.rs:DiskChatHistory], [coalesce.rs:Inner]).
- Async runtime: `tokio::spawn` for the background worker and `spawn_blocking` for `ChatHistory::save`. `CoalescingPersister::new` must be called inside a Tokio runtime ([coalesce.rs:CoalescingPersister::new]).
- Environment read: `$HOME` for `~` expansion ([lib.rs:resolve_tilde]).
- Logging: `tracing` warn, info and debug events for migration, save, rotate and checkpoint failures.

## Verification
- Run: `cargo test -p dyson-persistence` (from `dyson/`)
