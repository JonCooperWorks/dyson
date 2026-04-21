// ===========================================================================
// Workspace migration — upgrades workspace directories to current format.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines a declarative migration chain that transforms workspace
//   directories from any older version to the current version.  Each
//   migration is a sequence of filesystem-level `Step` operations.
//
// How it works:
//   1. `FilesystemWorkspace::load()` calls `migrate(path)` before reading files.
//   2. `migrate()` reads `.workspace_version` (default: 0 if missing).
//   3. It runs each migration's steps in order, skipping versions
//      below the current workspace version.
//   4. Steps are atomic filesystem operations — each one does exactly
//      one thing and is easy to audit.
//   5. After migration, `.workspace_version` is updated.
//
// Version detection:
//   Directories without a `.workspace_version` file are treated as version 0.
//   This means existing filesystem/TARS workspaces enter the chain at v0 and
//   get migrated forward automatically.  Content defaults (USER.md, etc.)
//   are handled separately by `ensure_defaults()` — migrations only handle
//   structural changes.
//
// Adding a new migration:
//   1. Bump `CURRENT_WORKSPACE_VERSION`.
//   2. Add a `Migration` to the `migrations()` vec with the new steps.
//   3. That's it — the chain handles the rest.
//
// Step operations:
//   - CreateDir(path)      — create a directory (and parents). No-op if exists.
//   - Rename(from, to)     — rename/move a file. No-op if source missing.
//   - SkipIf(path)         — skip remaining steps if path exists
//   - BailIf(path, msg)    — error if path exists (ambiguous state)
// ===========================================================================

use std::path::Path;

use crate::error::{DysonError, Result};

/// Current workspace version.  Bump this when adding a new migration.
pub const CURRENT_WORKSPACE_VERSION: u64 = 3;

/// Version file name.
const VERSION_FILE: &str = ".workspace_version";

// ---------------------------------------------------------------------------
// Step — a single atomic operation in a workspace migration.
// ---------------------------------------------------------------------------

/// A single declarative step in a workspace migration chain.
///
/// Each step is a well-defined filesystem operation.  No arbitrary code —
/// every possible operation is enumerated here.
enum Step {
    /// Create a directory relative to workspace root.  Creates parents.
    /// No-op if the directory already exists.
    CreateDir(&'static str),

    /// Rename a file or directory (from, to), relative to workspace root.
    /// No-op if source doesn't exist.
    #[allow(dead_code)]
    Rename(&'static str, &'static str),

    /// Skip all remaining steps in this migration if a path exists.
    /// Used for idempotency.
    #[allow(dead_code)]
    SkipIf(&'static str),

    /// Bail with an error if a path exists (ambiguous state).
    #[allow(dead_code)]
    BailIf(&'static str, &'static str),

    /// Promote flat files in a directory to subdirectories.
    ///
    /// Scans `dir` for files with extension `ext`, creates a subdirectory
    /// named after each file's stem, and moves the file into it as `target`.
    ///
    /// Example: `PromoteFilesToDirs { dir: "skills", ext: "md", target: "SKILL.md" }`
    /// turns `skills/code-review.md` into `skills/code-review/SKILL.md`.
    ///
    /// Skips files where the target directory already exists (idempotent).
    PromoteFilesToDirs {
        dir: &'static str,
        ext: &'static str,
        target: &'static str,
    },
}

/// A versioned migration: a description + a list of steps.
struct Migration {
    from_version: u64,
    description: &'static str,
    steps: &'static [Step],
}

// ---------------------------------------------------------------------------
// Migration definitions
// ---------------------------------------------------------------------------

/// All migrations, in order.
///
/// To add a new migration:
/// 1. Bump `CURRENT_WORKSPACE_VERSION` above.
/// 2. Add a `Migration` here with `from_version` = old CURRENT_WORKSPACE_VERSION.
/// 3. Define the steps using the Step enum.
const fn migrations() -> &'static [Migration] {
    &[
        // v0 → v1: filesystem format → Dyson workspace.
        //
        // Structural changes only — new default files (USER.md, HEARTBEAT.md)
        // are created by ensure_defaults(), not here.
        Migration {
            from_version: 0,
            description: "Create memory/notes/ directory for Tier 2 overflow",
            steps: &[Step::CreateDir("memory/notes")],
        },
        // v1 → v2: Promote flat skill files to directories.
        //
        // skills/code-review.md → skills/code-review/SKILL.md
        //
        // This enables each skill to have its own directory for references,
        // scripts, and examples alongside the SKILL.md file.
        Migration {
            from_version: 1,
            description: "Promote skills/*.md to skills/<name>/SKILL.md directories",
            steps: &[Step::PromoteFilesToDirs {
                dir: "skills",
                ext: "md",
                target: "SKILL.md",
            }],
        },
        // v2 → v3: Create knowledge base directory structure.
        //
        // kb/raw/  — source material (articles, papers, notes)
        // kb/wiki/ — compiled articles (agent-maintained on request)
        Migration {
            from_version: 2,
            description: "Create kb/ directory structure for knowledge base",
            steps: &[
                Step::CreateDir("kb"),
                Step::CreateDir("kb/raw"),
                Step::CreateDir("kb/wiki"),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read the workspace version from `.workspace_version` (0 if missing).
pub fn read_version(workspace_path: &Path) -> u64 {
    let version_path = workspace_path.join(VERSION_FILE);
    std::fs::read_to_string(version_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write the workspace version to `.workspace_version`.
fn write_version(workspace_path: &Path, version: u64) -> Result<()> {
    let version_path = workspace_path.join(VERSION_FILE);
    std::fs::write(&version_path, format!("{version}\n")).map_err(|e| {
        DysonError::Config(format!(
            "cannot write workspace version to {}: {e}",
            version_path.display()
        ))
    })
}

/// Run all applicable workspace migrations.
///
/// Returns `true` if any migration was applied (caller should log).
/// Errors if a migration can't be applied automatically.
pub fn migrate(workspace_path: &Path) -> Result<bool> {
    let version = read_version(workspace_path);

    if version > CURRENT_WORKSPACE_VERSION {
        return Err(DysonError::Config(format!(
            "workspace version {version} is newer than this version of Dyson (max {CURRENT_WORKSPACE_VERSION}).  \
             Please upgrade Dyson."
        )));
    }

    if version == CURRENT_WORKSPACE_VERSION {
        return Ok(false);
    }

    let mut current = version;
    for migration in migrations() {
        if migration.from_version < current {
            continue;
        }
        if migration.from_version != current {
            return Err(DysonError::Config(format!(
                "workspace migration gap: workspace is at version {current} but next migration is from {}",
                migration.from_version,
            )));
        }

        tracing::info!(
            from = migration.from_version,
            to = migration.from_version + 1,
            description = migration.description,
            "applying workspace migration"
        );

        apply_steps(workspace_path, migration.steps)?;
        current = migration.from_version + 1;
    }

    write_version(workspace_path, current)?;

    Ok(true)
}

// ---------------------------------------------------------------------------
// Step execution
// ---------------------------------------------------------------------------

/// Execute a list of steps against a workspace directory.
fn apply_steps(workspace_path: &Path, steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::CreateDir(path) => {
                let full = workspace_path.join(path);
                std::fs::create_dir_all(&full).map_err(|e| {
                    DysonError::Config(format!(
                        "workspace migration: cannot create {}: {e}",
                        full.display()
                    ))
                })?;
            }
            Step::Rename(from, to) => {
                let src = workspace_path.join(from);
                let dst = workspace_path.join(to);
                if src.exists() {
                    // Ensure parent of destination exists.
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            DysonError::Config(format!(
                                "workspace migration: cannot create parent for {}: {e}",
                                dst.display()
                            ))
                        })?;
                    }
                    std::fs::rename(&src, &dst).map_err(|e| {
                        DysonError::Config(format!(
                            "workspace migration: cannot rename {} to {}: {e}",
                            src.display(),
                            dst.display()
                        ))
                    })?;
                }
            }
            Step::SkipIf(path) => {
                if workspace_path.join(path).exists() {
                    return Ok(());
                }
            }
            Step::BailIf(path, message) => {
                if workspace_path.join(path).exists() {
                    return Err(DysonError::Config((*message).into()));
                }
            }
            Step::PromoteFilesToDirs { dir, ext, target } => {
                let full_dir = workspace_path.join(dir);
                if !full_dir.is_dir() {
                    continue;
                }
                let entries: Vec<_> = std::fs::read_dir(&full_dir)
                    .map_err(|e| {
                        DysonError::Config(format!(
                            "workspace migration: cannot read {}: {e}",
                            full_dir.display()
                        ))
                    })?
                    .filter_map(std::result::Result::ok)
                    .collect();

                for entry in entries {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let file_ext = path.extension().and_then(|e| e.to_str());
                    if file_ext != Some(ext) {
                        continue;
                    }
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };

                    let sub_dir = full_dir.join(&stem);
                    if sub_dir.exists() {
                        // Already promoted — skip.
                        continue;
                    }

                    std::fs::create_dir_all(&sub_dir).map_err(|e| {
                        DysonError::Config(format!(
                            "workspace migration: cannot create {}: {e}",
                            sub_dir.display()
                        ))
                    })?;

                    let dest = sub_dir.join(target);
                    std::fs::rename(&path, &dest).map_err(|e| {
                        DysonError::Config(format!(
                            "workspace migration: cannot rename {} to {}: {e}",
                            path.display(),
                            dest.display()
                        ))
                    })?;

                    tracing::info!(
                        from = %path.display(),
                        to = %dest.display(),
                        "promoted skill file to directory"
                    );
                }
            }
        }
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dyson-ws-migrate-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_version_missing_file_returns_zero() {
        let dir = temp_dir("read-missing");
        assert_eq!(read_version(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_write_version_roundtrip() {
        let dir = temp_dir("read-write");
        write_version(&dir, 42).unwrap();
        assert_eq!(read_version(&dir), 42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn already_current_version_returns_false() {
        let dir = temp_dir("already-current");
        write_version(&dir, CURRENT_WORKSPACE_VERSION).unwrap();
        let applied = migrate(&dir).unwrap();
        assert!(!applied);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn future_version_errors() {
        let dir = temp_dir("future");
        write_version(&dir, CURRENT_WORKSPACE_VERSION + 1).unwrap();
        let err = migrate(&dir).unwrap_err();
        assert!(err.to_string().contains("newer than this version"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v0_to_v1_creates_notes_dir() {
        let dir = temp_dir("v0-to-v1");
        // Create the memory/ dir (as load() would).
        std::fs::create_dir_all(dir.join("memory")).unwrap();

        // No version file = v0.
        assert_eq!(read_version(&dir), 0);

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        // memory/notes/ created.
        assert!(dir.join("memory/notes").is_dir());

        // Version stamped.
        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v0_to_v1_idempotent() {
        let dir = temp_dir("idempotent");
        std::fs::create_dir_all(dir.join("memory")).unwrap();

        // Run once.
        assert!(migrate(&dir).unwrap());
        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        // Run again — should be no-op.
        assert!(!migrate(&dir).unwrap());
        assert!(dir.join("memory/notes").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v0_to_v1_notes_dir_already_exists() {
        let dir = temp_dir("notes-exists");
        std::fs::create_dir_all(dir.join("memory/notes")).unwrap();

        // Should succeed even if memory/notes/ already exists (CreateDir is no-op).
        let applied = migrate(&dir).unwrap();
        assert!(applied);
        assert!(dir.join("memory/notes").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_skip_if_skips_remaining() {
        let dir = temp_dir("skip-if");
        std::fs::write(dir.join("marker"), "").unwrap();

        // SkipIf should prevent CreateDir from running.
        let steps = &[Step::SkipIf("marker"), Step::CreateDir("should-not-exist")];
        apply_steps(&dir, steps).unwrap();
        assert!(!dir.join("should-not-exist").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_bail_if_errors() {
        let dir = temp_dir("bail-if");
        std::fs::write(dir.join("conflict"), "").unwrap();

        let steps = &[Step::BailIf("conflict", "conflicting file exists")];
        let err = apply_steps(&dir, steps).unwrap_err();
        assert!(err.to_string().contains("conflicting file exists"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_bail_if_passes_when_missing() {
        let dir = temp_dir("bail-if-missing");
        let steps = &[Step::BailIf("nope", "should not fire")];
        apply_steps(&dir, steps).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_rename_moves_file() {
        let dir = temp_dir("rename");
        std::fs::write(dir.join("old.md"), "content").unwrap();

        let steps = &[Step::Rename("old.md", "new.md")];
        apply_steps(&dir, steps).unwrap();

        assert!(!dir.join("old.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("new.md")).unwrap(),
            "content"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_rename_noop_when_missing() {
        let dir = temp_dir("rename-missing");

        let steps = &[Step::Rename("gone.md", "dest.md")];
        apply_steps(&dir, steps).unwrap();
        assert!(!dir.join("dest.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_rename_creates_parent_dirs() {
        let dir = temp_dir("rename-parents");
        std::fs::write(dir.join("file.md"), "data").unwrap();

        let steps = &[Step::Rename("file.md", "sub/dir/file.md")];
        apply_steps(&dir, steps).unwrap();

        assert!(!dir.join("file.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/dir/file.md")).unwrap(),
            "data"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------
    // v1 → v2: PromoteFilesToDirs tests
    // -------------------------------------------------------------------

    #[test]
    fn v1_to_v2_promotes_skill_files() {
        let dir = temp_dir("v1-to-v2");
        write_version(&dir, 1).unwrap();
        std::fs::create_dir_all(dir.join("skills")).unwrap();

        // Create flat skill files.
        std::fs::write(
            dir.join("skills/code-review.md"),
            "---\nname: code-review\n---\n\nReview code.",
        )
        .unwrap();
        std::fs::write(
            dir.join("skills/deploy.md"),
            "---\nname: deploy\n---\n\nDeploy things.",
        )
        .unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        // Flat files should be gone.
        assert!(!dir.join("skills/code-review.md").exists());
        assert!(!dir.join("skills/deploy.md").exists());

        // Directory structure should exist.
        assert!(dir.join("skills/code-review/SKILL.md").is_file());
        assert!(dir.join("skills/deploy/SKILL.md").is_file());

        // Content preserved.
        let content = std::fs::read_to_string(dir.join("skills/code-review/SKILL.md")).unwrap();
        assert!(content.contains("Review code."));

        // Version updated.
        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_to_v2_skips_already_promoted() {
        let dir = temp_dir("v1-to-v2-skip");
        write_version(&dir, 1).unwrap();
        std::fs::create_dir_all(dir.join("skills/existing")).unwrap();
        std::fs::write(dir.join("skills/existing/SKILL.md"), "already here").unwrap();
        // Also a flat file named "existing.md" — should be skipped because
        // skills/existing/ already exists.
        std::fs::write(dir.join("skills/existing.md"), "flat version").unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        // The directory version should be preserved (not overwritten).
        let content = std::fs::read_to_string(dir.join("skills/existing/SKILL.md")).unwrap();
        assert_eq!(content, "already here");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_to_v2_ignores_non_md_files() {
        let dir = temp_dir("v1-to-v2-nonmd");
        write_version(&dir, 1).unwrap();
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::write(dir.join("skills/notes.txt"), "not a skill").unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        // .txt file should be untouched.
        assert!(dir.join("skills/notes.txt").is_file());
        assert!(!dir.join("skills/notes").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_to_v2_no_skills_dir() {
        let dir = temp_dir("v1-to-v2-noskills");
        write_version(&dir, 1).unwrap();

        // No skills/ directory at all — should succeed.
        let applied = migrate(&dir).unwrap();
        assert!(applied);
        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_to_v3_creates_kb_dirs() {
        let dir = temp_dir("v2-to-v3");
        write_version(&dir, 2).unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        assert!(dir.join("kb").is_dir());
        assert!(dir.join("kb/raw").is_dir());
        assert!(dir.join("kb/wiki").is_dir());

        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_to_v3_idempotent_when_kb_exists() {
        let dir = temp_dir("v2-to-v3-exists");
        write_version(&dir, 2).unwrap();
        std::fs::create_dir_all(dir.join("kb/raw")).unwrap();
        std::fs::create_dir_all(dir.join("kb/wiki")).unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);
        assert!(dir.join("kb/raw").is_dir());
        assert!(dir.join("kb/wiki").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v0_to_v3_full_chain() {
        let dir = temp_dir("v0-to-v3");
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::write(
            dir.join("skills/test.md"),
            "---\nname: test\n---\n\nTest skill.",
        )
        .unwrap();

        let applied = migrate(&dir).unwrap();
        assert!(applied);

        // v0→v1: memory/notes/ created.
        assert!(dir.join("memory/notes").is_dir());

        // v1→v2: skill promoted.
        assert!(dir.join("skills/test/SKILL.md").is_file());

        // v2→v3: kb/ created.
        assert!(dir.join("kb").is_dir());
        assert!(dir.join("kb/raw").is_dir());
        assert!(dir.join("kb/wiki").is_dir());

        assert_eq!(read_version(&dir), CURRENT_WORKSPACE_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
