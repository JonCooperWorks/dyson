// ===========================================================================
// Hot reload — re-read config and workspace when files change on disk.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Watches files by mtime (last modification time).  Before each agent
//   turn, the controller calls `check()` — if any watched file changed,
//   it signals a reload.
//
// Watches two things:
//   1. dyson.json — agent settings, sandbox config, etc.
//   2. Workspace files — SOUL.md, MEMORY.md, IDENTITY.md, etc.
//
// Why mtime instead of a file watcher (inotify/FSEvents)?
//   - Zero dependencies
//   - Works on all platforms
//   - Polling once per agent turn is cheap (a few stat() syscalls)
//
// Debounce:
//   Editors often save files in multiple steps (write to temp file, then
//   rename).  If we reload on the first mtime change, we may read a
//   partial or empty file.  The debounce requires the mtime to be stable
//   for `DEBOUNCE_DURATION` before we consider the change settled and
//   trigger a reload.
//
//   Flow:
//     1. check() detects mtime changed → record the change time, but
//        do NOT reload yet
//     2. Next check() call — if mtime is still the same AND enough time
//        has passed since the change was first detected → reload
//     3. If mtime changed again → reset the debounce timer
// ===========================================================================

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Settings;
use crate::error::Result;

/// How long the mtime must be stable before we reload.
///
/// 500ms is long enough for write-then-rename (typically <10ms) and
/// short enough that the user doesn't notice a delay.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// HotReloader
// ---------------------------------------------------------------------------

/// Watches config and workspace files for changes.
///
/// Call `check()` before each agent turn.  Returns `true` if anything
/// changed (config or workspace files), signaling the controller to
/// rebuild the agent.
///
/// Changes are debounced: the mtime must be stable for 500ms before a
/// reload is triggered.  This prevents reading partial files when editors
/// write in multiple steps.
pub struct HotReloader {
    /// Watched files and their last known mtimes.
    watched: HashMap<PathBuf, Option<SystemTime>>,

    /// Path to the config file (for reloading settings).
    config_path: Option<PathBuf>,

    /// When we first detected a change (None = no pending change).
    /// Reset to None after a successful reload or if mtime changes again.
    pending_change: Option<PendingChange>,
}

/// Tracks a detected-but-not-yet-settled file change.
struct PendingChange {
    /// Wall-clock instant when we first noticed the mtime difference.
    detected_at: Instant,
    /// Snapshot of mtimes at detection time, so we can tell if the file
    /// changed again (which resets the debounce).
    mtimes: HashMap<PathBuf, Option<SystemTime>>,
}

impl HotReloader {
    /// Create a reloader that watches the config file and workspace directory.
    pub fn new(config_path: Option<&Path>, workspace_path: Option<&Path>) -> Self {
        let mut watched = HashMap::new();

        // Watch the config file.
        if let Some(p) = config_path {
            let mtime = Self::get_mtime(p);
            watched.insert(p.to_path_buf(), mtime);
        }

        // Watch workspace .md files.
        if let Some(ws) = workspace_path {
            Self::scan_workspace(ws, &mut watched);
        }

        Self {
            watched,
            config_path: config_path.map(|p| p.to_path_buf()),
            pending_change: None,
        }
    }

    /// Check if any watched file changed, waiting for the debounce to settle.
    ///
    /// Unlike `check_nonblocking()`, this method sleeps until the debounce
    /// period elapses when a change is detected.  This ensures a single call
    /// is sufficient to pick up changes — important for interactive loops
    /// where there's only one check per turn.
    ///
    /// Returns `Ok((true, Some(settings)))` if the config file changed.
    /// Returns `Ok((true, None))` if only workspace files changed.
    /// Returns `Ok((false, None))` if nothing changed.
    ///
    /// Uses `tokio::time::sleep` to avoid blocking the async runtime.
    pub async fn check(&mut self) -> Result<(bool, Option<Settings>)> {
        let result = self.check_nonblocking()?;
        if result.0 {
            return Ok(result);
        }

        // If we have a pending change, wait for the debounce to settle.
        if let Some(ref pending) = self.pending_change {
            let elapsed = pending.detected_at.elapsed();
            if elapsed < DEBOUNCE_DURATION {
                tokio::time::sleep(DEBOUNCE_DURATION - elapsed).await;
            }
            return self.check_nonblocking();
        }

        Ok(result)
    }

    /// Check if any watched file changed (non-blocking, with debounce).
    ///
    /// Returns `Ok((true, Some(settings)))` if the config file changed.
    /// Returns `Ok((true, None))` if only workspace files changed.
    /// Returns `Ok((false, None))` if nothing changed or debounce pending.
    fn check_nonblocking(&mut self) -> Result<(bool, Option<Settings>)> {
        // Snapshot current mtimes.  Re-use the watched keys to avoid
        // cloning every PathBuf on each check.
        let current_mtimes: HashMap<PathBuf, Option<SystemTime>> = self
            .watched
            .keys()
            .map(|p| (p.clone(), Self::get_mtime(p)))
            .collect();

        // Check if anything differs from our last-committed state.
        let has_diff = self
            .watched
            .iter()
            .any(|(path, last_mtime)| current_mtimes.get(path) != Some(last_mtime));

        if !has_diff {
            // No changes at all — also clear any pending debounce since
            // the file may have reverted.
            if let Some(ref pending) = self.pending_change {
                // Check if the pending mtimes still match current — if so,
                // the debounce timer is still valid.  If not, clear it.
                let pending_matches = pending
                    .mtimes
                    .iter()
                    .all(|(p, m)| current_mtimes.get(p) == Some(m));
                if !pending_matches {
                    self.pending_change = None;
                }
            }

            // Check if a pending change has settled.
            if let Some(ref pending) = self.pending_change
                && pending.detected_at.elapsed() >= DEBOUNCE_DURATION
            {
                // Debounce complete — commit the change.
                return self.commit_reload(&current_mtimes);
            }

            return Ok((false, None));
        }

        // Something changed.  Start or reset the debounce timer.
        match self.pending_change {
            Some(ref pending) => {
                // Already have a pending change.  Did the mtime change
                // again (i.e., different from what we recorded at detection)?
                let changed_again = pending
                    .mtimes
                    .iter()
                    .any(|(p, m)| current_mtimes.get(p) != Some(m));
                if changed_again {
                    // Reset the debounce — file is still being written.
                    self.pending_change = Some(PendingChange {
                        detected_at: Instant::now(),
                        mtimes: current_mtimes,
                    });
                }
                // Otherwise, debounce is still counting down from the
                // first detection.  Check if it's settled.
                else if pending.detected_at.elapsed() >= DEBOUNCE_DURATION {
                    return self.commit_reload(&current_mtimes);
                }
            }
            None => {
                // First detection — start the debounce timer.
                self.pending_change = Some(PendingChange {
                    detected_at: Instant::now(),
                    mtimes: current_mtimes,
                });
            }
        }

        Ok((false, None))
    }

    /// Commit the reload: update watched mtimes, clear pending state,
    /// and reload config if applicable.
    fn commit_reload(
        &mut self,
        current_mtimes: &HashMap<PathBuf, Option<SystemTime>>,
    ) -> Result<(bool, Option<Settings>)> {
        // Check if the config file specifically changed (not just workspace files).
        let config_changed = self
            .config_path
            .as_ref()
            .is_some_and(|p| current_mtimes.get(p) != self.watched.get(p));

        // Log which files changed.
        for (path, last_mtime) in &self.watched {
            if current_mtimes.get(path) != Some(last_mtime) {
                tracing::info!(path = %path.display(), "file changed");
            }
        }

        // Update watched mtimes.
        for (path, mtime) in current_mtimes {
            if let Some(entry) = self.watched.get_mut(path) {
                *entry = *mtime;
            }
        }

        self.pending_change = None;

        // Only reload settings when the config file itself changed.
        let mut new_settings = None;
        if config_changed
            && let Some(ref config_path) = self.config_path
        {
            match crate::config::loader::load_settings(Some(config_path)) {
                Ok(s) => new_settings = Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "config reload failed — keeping old");
                }
            }
        }

        Ok((true, new_settings))
    }

    fn get_mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    fn scan_workspace(ws_path: &Path, watched: &mut HashMap<PathBuf, Option<SystemTime>>) {
        // Watch top-level .md files.
        if let Ok(entries) = std::fs::read_dir(ws_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let p = entry.path();
                    let mtime = Self::get_mtime(&p);
                    watched.insert(p, mtime);
                }
            }
        }

        // Watch memory/ journal files.
        let memory_dir = ws_path.join("memory");
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let p = entry.path();
                    let mtime = Self::get_mtime(&p);
                    watched.insert(p, mtime);
                }
            }
        }

        // Watch skills/ directory (mtime changes when dirs are added/removed)
        // and each skill's SKILL.md for content changes.
        let skills_dir = ws_path.join("skills");
        if skills_dir.is_dir() {
            let mtime = Self::get_mtime(&skills_dir);
            watched.insert(skills_dir.clone(), mtime);

            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let skill_md = entry.path().join("SKILL.md");
                    if skill_md.is_file() {
                        let mtime = Self::get_mtime(&skill_md);
                        watched.insert(skill_md, mtime);
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dyson-hot-reload-test-{}-{}",
            name,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn no_change_returns_false() {
        let dir = temp_dir("no-change");
        let config = dir.join("dyson.json");
        std::fs::write(&config, r#"{"agent":{}}"#).unwrap();

        let mut reloader = HotReloader::new(Some(&config), None);
        let (changed, _) = reloader.check().await.unwrap();
        assert!(!changed, "should not report change on first check");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn change_is_debounced() {
        let dir = temp_dir("debounce");
        let config = dir.join("dyson.json");
        std::fs::write(&config, r#"{"agent":{}}"#).unwrap();

        let mut reloader = HotReloader::new(Some(&config), None);

        // Modify the file.
        std::thread::sleep(Duration::from_millis(50));
        {
            let mut f = std::fs::File::create(&config).unwrap();
            write!(f, r#"{{"agent":{{"model":"test"}}}}"#).unwrap();
        }

        // Non-blocking check — should NOT reload yet (debounce).
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(!changed, "change should be debounced, not immediate");

        // Wait for debounce to settle.
        std::thread::sleep(DEBOUNCE_DURATION + Duration::from_millis(50));

        // Now it should report the change.
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(changed, "change should be reported after debounce");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn blocking_check_waits_for_debounce() {
        let dir = temp_dir("blocking");
        let config = dir.join("dyson.json");
        std::fs::write(&config, r#"{"agent":{}}"#).unwrap();

        let mut reloader = HotReloader::new(Some(&config), None);

        // Modify the file.
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&config, r#"{"agent":{"model":"test"}}"#).unwrap();

        // Async check should wait for debounce and report the change
        // in a single call.
        let (changed, _) = reloader.check().await.unwrap();
        assert!(
            changed,
            "async check should wait for debounce and report change"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_change_triggers_reload_attempt() {
        let dir = temp_dir("config-reload");
        let config = dir.join("dyson.json");
        // Write a valid config.
        std::fs::write(&config, r#"{"agent":{}}"#).unwrap();

        let mut reloader = HotReloader::new(Some(&config), None);

        // Modify the config file.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&config, r#"{"agent":{"max_iterations":5}}"#).unwrap();

        // Detect the change.
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(!changed, "should be debounced");

        // Wait for debounce to settle.
        std::thread::sleep(DEBOUNCE_DURATION + Duration::from_millis(50));

        // Should report changed=true.  Settings may or may not load
        // depending on environment (API key availability), but the
        // reload path is exercised.
        let (changed, _settings) = reloader.check_nonblocking().unwrap();
        assert!(changed, "config change should trigger reload");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_file_change_returns_no_settings() {
        let dir = temp_dir("ws-change");
        let ws_dir = dir.join("workspace");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let soul = ws_dir.join("SOUL.md");
        std::fs::write(&soul, "Be helpful.").unwrap();

        let mut reloader = HotReloader::new(None, Some(&ws_dir));

        // Modify a workspace file.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&soul, "Be very helpful.").unwrap();

        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(!changed, "should be debounced");

        std::thread::sleep(DEBOUNCE_DURATION + Duration::from_millis(50));

        let (changed, settings) = reloader.check_nonblocking().unwrap();
        assert!(changed, "workspace change should trigger reload");
        assert!(
            settings.is_none(),
            "workspace-only change should not produce Settings"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_watched_files_returns_false() {
        let mut reloader = HotReloader::new(None, None);
        let (changed, settings) = reloader.check_nonblocking().unwrap();
        assert!(!changed);
        assert!(settings.is_none());
    }

    #[test]
    fn rapid_writes_reset_debounce() {
        let dir = temp_dir("rapid-writes");
        let config = dir.join("dyson.json");
        std::fs::write(&config, r#"{"agent":{}}"#).unwrap();

        let mut reloader = HotReloader::new(Some(&config), None);

        // First write.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&config, r#"{"agent":{"model":"v1"}}"#).unwrap();
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(!changed);

        // Second write before debounce expires — should reset timer.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&config, r#"{"agent":{"model":"v2"}}"#).unwrap();
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(!changed, "debounce should have reset");

        // Wait for debounce after final write.
        std::thread::sleep(DEBOUNCE_DURATION + Duration::from_millis(50));
        let (changed, _) = reloader.check_nonblocking().unwrap();
        assert!(changed, "should report after debounce settled");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
