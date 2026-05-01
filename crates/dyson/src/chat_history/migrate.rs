// ===========================================================================
// Chat directory migration — upgrades the chats dir to the current layout.
//
// Mirrors the pattern in `workspace/migrate.rs`: a version file
// (`.chats_version`), a declarative sequence of `Step`s, and an
// idempotent `migrate()` entry point called at startup.
//
// v0 → v1: flat layout → per-chat subdirectory.
//
//   Before (v0):
//     chats/
//       c-0001.json
//       c-0001.2026-03-19T14-30-00.json
//       c-0001_feedback.json
//       c-0001_media/
//       artefacts/{aN}.body + {aN}.meta.json   (meta carries chat_id)
//
//   After (v1):
//     chats/
//       c-0001/
//         transcript.json
//         archives/2026-03-19T14-30-00.json
//         feedback.json
//         media/
//         artefacts/
//
// Delete-cascade is then a single `remove_dir_all({chat_id})`, and
// the "orphan artefact tagged with a reused chat_id" class of bugs
// is eliminated by layout.
// ===========================================================================

use std::path::{Path, PathBuf};

use crate::error::{DysonError, Result};

/// Current chats-directory version.  Bump when adding a new migration.
pub const CURRENT_CHATS_VERSION: u64 = 1;

/// Version file name.
const VERSION_FILE: &str = ".chats_version";

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// One atomic operation in the chat-dir migration chain.
enum Step {
    /// Walk the root directory and, for every file whose name matches
    /// a known flat-layout pattern, relocate it into the per-chat
    /// subdirectory it belongs to.  Idempotent — files already in
    /// subdirs are left alone.
    FlattenToPerChatDirs,

    /// Fan the shared `artefacts/` directory out into each chat's
    /// subdir using the `chat_id` field inside each `*.meta.json`.
    /// Idempotent — missing or malformed meta files are skipped, not
    /// fatal.  Removes the now-empty shared dir afterwards.
    FanOutSharedArtefacts,
}

struct Migration {
    from_version: u64,
    description: &'static str,
    steps: &'static [Step],
}

const fn migrations() -> &'static [Migration] {
    &[Migration {
        from_version: 0,
        description: "Flatten chats dir into per-chat subdirectories",
        steps: &[Step::FlattenToPerChatDirs, Step::FanOutSharedArtefacts],
    }]
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

fn read_version(dir: &Path) -> u64 {
    std::fs::read_to_string(dir.join(VERSION_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| default_version_for(dir))
}

/// Decide the starting version for a dir with no `.chats_version`.
/// A brand-new (empty) dir is already at the current layout — no
/// legacy files to promote.  An older dir with flat-layout files
/// starts at 0 so the migration chain runs.
fn default_version_for(dir: &Path) -> u64 {
    match std::fs::read_dir(dir) {
        Ok(it) => {
            let has_flat_artifacts = it.flatten().any(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                // Any flat-layout telltale: {id}.json, {id}.TIMESTAMP.json,
                // {id}_feedback.json, {id}_media, or a shared artefacts dir.
                s.ends_with(".json")
                    || s.ends_with("_feedback.json")
                    || s.ends_with("_media")
                    || s == "artefacts"
            });
            if has_flat_artifacts {
                0
            } else {
                CURRENT_CHATS_VERSION
            }
        }
        Err(_) => CURRENT_CHATS_VERSION,
    }
}

fn write_version(dir: &Path, version: u64) -> Result<()> {
    let path = dir.join(VERSION_FILE);
    std::fs::write(&path, format!("{version}\n")).map_err(|e| {
        DysonError::Config(format!(
            "cannot write chats version to {}: {e}",
            path.display()
        ))
    })
}

/// Run any applicable migrations.  Returns `true` if anything moved.
pub fn migrate(dir: &Path) -> Result<bool> {
    let version = read_version(dir);
    if version > CURRENT_CHATS_VERSION {
        return Err(DysonError::Config(format!(
            "chats dir version {version} is newer than this build (max {CURRENT_CHATS_VERSION})"
        )));
    }
    if version == CURRENT_CHATS_VERSION {
        return Ok(false);
    }

    let mut current = version;
    for migration in migrations() {
        if migration.from_version < current {
            continue;
        }
        if migration.from_version != current {
            return Err(DysonError::Config(format!(
                "chats migration gap: dir at {current}, next from {}",
                migration.from_version,
            )));
        }
        tracing::info!(
            from = migration.from_version,
            to = migration.from_version + 1,
            description = migration.description,
            dir = %dir.display(),
            "applying chats migration",
        );
        for step in migration.steps {
            apply_step(dir, step)?;
        }
        current = migration.from_version + 1;
        write_version(dir, current)?;
    }
    Ok(true)
}

fn apply_step(dir: &Path, step: &Step) -> Result<()> {
    match step {
        Step::FlattenToPerChatDirs => flatten_to_per_chat_dirs(dir),
        Step::FanOutSharedArtefacts => fan_out_shared_artefacts(dir),
    }
}

// ---------------------------------------------------------------------------
// Step implementations
// ---------------------------------------------------------------------------

fn flatten_to_per_chat_dirs(dir: &Path) -> Result<()> {
    let entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };

        // {id}_media/ → {id}/media/
        if path.is_dir()
            && let Some(id) = name.strip_suffix("_media")
        {
            let target = dir.join(id).join("media");
            move_path(&path, &target);
            continue;
        }

        // Skip subdirectories we've already created or that existed as
        // per-chat subdirs (e.g. a re-run).
        if path.is_dir() {
            continue;
        }

        // {id}_feedback.json → {id}/feedback.json
        if path.is_file()
            && let Some(id) = name.strip_suffix("_feedback.json")
        {
            let target = dir.join(id).join("feedback.json");
            move_path(&path, &target);
            continue;
        }

        // {id}.json and {id}.TIMESTAMP.json
        if path.is_file()
            && let Some(stem) = name.strip_suffix(".json")
        {
            if stem.starts_with('.') {
                // `.chats_version` or similar hidden marker — leave alone.
                continue;
            }
            if let Some((id, ts)) = stem.split_once('.') {
                // Archive.
                let target = dir.join(id).join("archives").join(format!("{ts}.json"));
                move_path(&path, &target);
            } else {
                // Current transcript.
                let target = dir.join(stem).join("transcript.json");
                move_path(&path, &target);
            }
        }
    }

    Ok(())
}

fn fan_out_shared_artefacts(dir: &Path) -> Result<()> {
    let shared = dir.join("artefacts");
    if !shared.is_dir() {
        return Ok(());
    }

    let entries: Vec<_> = match std::fs::read_dir(&shared) {
        Ok(it) => it.flatten().collect(),
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.ends_with(".meta.json") {
            continue;
        }
        let id = name.trim_end_matches(".meta.json").to_string();
        let meta_txt = match std::fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let meta: serde_json::Value = match serde_json::from_str(&meta_txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let chat_id = match meta.get("chat_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let target_dir = dir.join(&chat_id).join("artefacts");
        for suffix in ["body", "meta.json"] {
            let src = shared.join(format!("{id}.{suffix}"));
            if src.exists() {
                move_path(&src, &target_dir.join(format!("{id}.{suffix}")));
            }
        }
    }

    // Remove the now-empty shared dir.  `remove_dir` only succeeds if
    // the dir is truly empty, which is what we want — any leftover
    // unattributed entries stay there for investigation.
    let _ = std::fs::remove_dir(&shared);
    Ok(())
}

fn move_path(src: &Path, dst: &PathBuf) {
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::rename(src, dst) {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            error = %e,
            "chats migration: rename failed",
        );
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dyson_chats_migrate_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn v0_flat_layout_migrates_to_per_chat_subdirs() {
        let dir = tmp("flat_to_per_chat");

        std::fs::write(dir.join("c-0001.json"), b"[]").unwrap();
        std::fs::write(dir.join("c-0001.2026-04-22T12-00-00.json"), b"[]").unwrap();
        std::fs::write(dir.join("c-0001_feedback.json"), b"[]").unwrap();
        std::fs::create_dir_all(dir.join("c-0001_media")).unwrap();
        std::fs::write(dir.join("c-0001_media").join("deadbeef.b64"), b"Zm9v").unwrap();

        let shared = dir.join("artefacts");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("a1.body"), b"# report").unwrap();
        std::fs::write(
            shared.join("a1.meta.json"),
            br#"{"chat_id":"c-0002","kind":"security_review","title":"t","mime_type":"text/markdown","created_at":0}"#,
        ).unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied, "migration should run on a v0 dir");

        assert!(dir.join("c-0001").join("transcript.json").exists());
        assert!(
            dir.join("c-0001")
                .join("archives")
                .join("2026-04-22T12-00-00.json")
                .exists()
        );
        assert!(dir.join("c-0001").join("feedback.json").exists());
        assert!(
            dir.join("c-0001")
                .join("media")
                .join("deadbeef.b64")
                .exists()
        );
        assert!(
            dir.join("c-0002")
                .join("artefacts")
                .join("a1.body")
                .exists()
        );
        assert!(!dir.join("artefacts").exists());

        assert_eq!(read_version(&dir), CURRENT_CHATS_VERSION);
    }

    #[test]
    fn empty_dir_is_flagged_current_version_without_file_ops() {
        let dir = tmp("empty");
        let applied = migrate(&dir).unwrap();
        assert!(!applied);
        // Empty dirs default to current version — no marker needed yet,
        // but writing it is fine if we choose to.
        assert_eq!(read_version(&dir), CURRENT_CHATS_VERSION);
    }

    #[test]
    fn already_migrated_dir_is_noop() {
        let dir = tmp("already");
        std::fs::create_dir_all(dir.join("c-0001")).unwrap();
        std::fs::write(dir.join("c-0001").join("transcript.json"), b"[]").unwrap();
        write_version(&dir, CURRENT_CHATS_VERSION).unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(!applied);
    }
}
