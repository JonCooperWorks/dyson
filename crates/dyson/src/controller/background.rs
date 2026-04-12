// ===========================================================================
// Background agent registry — tracks autonomous agents spawned by `/loop`.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Provides a registry for background agent tasks spawned by the `/loop`
//   command.  Each background agent runs in its own `tokio::spawn` task
//   with no tool-call-iteration limit, producing output to a log file.
//
// Why a registry?
//   Background agents are fire-and-forget (like dreams), but users need
//   observability: list running agents (`/agents`) and stop them (`/stop`).
//   The registry is the single source of truth for background agent state.
//
// Design:
//   - `Arc<BackgroundAgentRegistry>` is shared across controllers (same
//     pattern as `ClientRegistry`).
//   - Interior mutability via `std::sync::Mutex` — all operations are O(1)
//     HashMap lookups with no I/O, so a std Mutex is simpler than tokio's.
//   - Each agent gets a `CancellationToken` for cooperative `/stop`.
//   - Capacity is capped at `MAX_BACKGROUND_AGENTS` to prevent resource
//     exhaustion.
// ===========================================================================

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum number of concurrent background agents.  Matches
/// `MAX_CONCURRENT_DREAMS` to keep resource usage predictable.
const MAX_BACKGROUND_AGENTS: usize = 4;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Entry returned by [`BackgroundAgentRegistry::list()`] for rendering.
pub struct BackgroundAgentListEntry {
    pub id: u64,
    pub prompt_preview: String,
    pub elapsed: Duration,
    pub log_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Metadata about a single running background agent.
struct BackgroundAgentInfo {
    prompt_preview: String,
    started_at: Instant,
    cancel: CancellationToken,
    log_path: PathBuf,
    handle: Option<JoinHandle<()>>,
}

struct Inner {
    agents: HashMap<u64, BackgroundAgentInfo>,
    next_id: u64,
}

impl Inner {
    /// Remove entries whose tasks have finished.
    fn prune(&mut self) {
        self.agents.retain(|_, info| {
            info.handle.as_ref().is_none_or(|h| !h.is_finished())
        });
    }
}

// ---------------------------------------------------------------------------
// BackgroundAgentRegistry
// ---------------------------------------------------------------------------

/// Registry tracking all running background agents.
///
/// Shared across controllers via `Arc`.  Thread-safe via `std::sync::Mutex`
/// (operations are fast, never held across await points).
pub struct BackgroundAgentRegistry {
    inner: std::sync::Mutex<Inner>,
}

impl BackgroundAgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                agents: HashMap::new(),
                next_id: 1,
            }),
        }
    }

    /// Reserve an ID, register a background agent, and compute its log path.
    ///
    /// `log_path_fn` receives the allocated ID and returns the log path.
    /// This avoids a chicken-and-egg problem: the caller needs the ID to
    /// compute the path, and the registry needs the path at registration.
    ///
    /// Call [`set_handle()`] immediately after `tokio::spawn` to attach
    /// the task handle.
    pub fn allocate(
        &self,
        prompt_preview: String,
        cancel: CancellationToken,
        log_path_fn: impl FnOnce(u64) -> PathBuf,
    ) -> Result<(u64, PathBuf), String> {
        let mut inner = self.inner.lock().expect("BackgroundAgentRegistry poisoned");
        inner.prune();

        if inner.agents.len() >= MAX_BACKGROUND_AGENTS {
            return Err(format!(
                "too many background agents running (max {MAX_BACKGROUND_AGENTS})"
            ));
        }

        let id = inner.next_id;
        inner.next_id += 1;
        let log_path = log_path_fn(id);

        inner.agents.insert(
            id,
            BackgroundAgentInfo {
                prompt_preview,
                started_at: Instant::now(),
                cancel,
                log_path: log_path.clone(),
                handle: None,
            },
        );

        Ok((id, log_path))
    }

    /// Attach the `JoinHandle` for a previously allocated agent.
    ///
    /// Called immediately after `tokio::spawn` so the registry can track
    /// task completion.
    pub fn set_handle(&self, id: u64, handle: JoinHandle<()>) {
        let mut inner = self.inner.lock().expect("BackgroundAgentRegistry poisoned");
        if let Some(info) = inner.agents.get_mut(&id) {
            info.handle = Some(handle);
        }
    }

    /// List all running background agents.
    ///
    /// Prunes finished agents as a side effect (safety net for tasks that
    /// completed without calling `remove()`).
    pub fn list(&self) -> Vec<BackgroundAgentListEntry> {
        let mut inner = self.inner.lock().expect("BackgroundAgentRegistry poisoned");
        inner.prune();

        let mut entries: Vec<BackgroundAgentListEntry> = inner
            .agents
            .iter()
            .map(|(&id, info)| BackgroundAgentListEntry {
                id,
                prompt_preview: info.prompt_preview.clone(),
                elapsed: info.started_at.elapsed(),
                log_path: info.log_path.clone(),
            })
            .collect();

        // Sort by ID for stable display order.
        entries.sort_by_key(|e| e.id);
        entries
    }

    /// Cancel a running background agent by ID.
    ///
    /// Triggers cooperative cancellation via the agent's `CancellationToken`.
    /// The agent loop checks this token at each iteration boundary and breaks
    /// cleanly.
    pub fn stop(&self, id: u64) -> Result<(), String> {
        let inner = self.inner.lock().expect("BackgroundAgentRegistry poisoned");
        match inner.agents.get(&id) {
            Some(info) => {
                info.cancel.cancel();
                Ok(())
            }
            None => Err(format!("no background agent with ID {id}")),
        }
    }

    /// Remove a background agent from the registry.
    ///
    /// Called from within the spawned task when the agent finishes (success
    /// or error).
    pub fn remove(&self, id: u64) {
        let mut inner = self.inner.lock().expect("BackgroundAgentRegistry poisoned");
        inner.agents.remove(&id);
    }
}

impl Default for BackgroundAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn log_path(id: u64) -> PathBuf {
        PathBuf::from(format!("/tmp/{id}.log"))
    }

    #[test]
    fn allocate_and_list() {
        let reg = BackgroundAgentRegistry::new();
        let cancel = CancellationToken::new();
        let (id, path) = reg
            .allocate("test prompt".into(), cancel, log_path)
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(path, PathBuf::from("/tmp/1.log"));

        let entries = reg.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[0].prompt_preview, "test prompt");
        assert_eq!(entries[0].log_path, PathBuf::from("/tmp/1.log"));
    }

    #[test]
    fn remove_clears_entry() {
        let reg = BackgroundAgentRegistry::new();
        let (id, _) = reg.allocate("test".into(), CancellationToken::new(), log_path).unwrap();

        reg.remove(id);
        assert!(reg.list().is_empty());
    }

    #[test]
    fn stop_cancels_token() {
        let reg = BackgroundAgentRegistry::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let (id, _) = reg.allocate("test".into(), cancel, log_path).unwrap();

        assert!(!cancel_clone.is_cancelled());
        reg.stop(id).unwrap();
        assert!(cancel_clone.is_cancelled());
    }

    #[test]
    fn stop_unknown_id_errors() {
        let reg = BackgroundAgentRegistry::new();
        assert!(reg.stop(999).is_err());
    }

    #[test]
    fn capacity_limit() {
        let reg = BackgroundAgentRegistry::new();
        for _ in 0..MAX_BACKGROUND_AGENTS {
            reg.allocate("test".into(), CancellationToken::new(), log_path).unwrap();
        }
        let result = reg.allocate("overflow".into(), CancellationToken::new(), log_path);
        assert!(result.is_err());
    }

    #[test]
    fn ids_are_monotonic() {
        let reg = BackgroundAgentRegistry::new();
        let (id1, _) = reg.allocate("a".into(), CancellationToken::new(), log_path).unwrap();
        let (id2, _) = reg.allocate("b".into(), CancellationToken::new(), log_path).unwrap();
        assert!(id2 > id1);
    }

    #[tokio::test]
    async fn prune_removes_finished_tasks() {
        let reg = BackgroundAgentRegistry::new();
        let (id, _) = reg.allocate("done".into(), CancellationToken::new(), log_path).unwrap();

        // Spawn a task that completes immediately.
        let handle = tokio::spawn(async {});
        handle.await.unwrap(); // Wait for it to finish.

        // Manually set a finished handle.
        let finished_handle = tokio::spawn(async {});
        finished_handle.await.unwrap();
        reg.set_handle(id, tokio::spawn(async {}));

        // Give the runtime a moment to mark it finished.
        tokio::task::yield_now().await;

        // list() should prune the finished task.
        let entries = reg.list();
        assert!(entries.is_empty(), "finished task should be pruned");
    }

    #[test]
    fn prune_frees_capacity() {
        let reg = BackgroundAgentRegistry::new();

        // Fill to capacity with handle-less entries (no handle = never pruned
        // by is_finished, but remove() works).
        for _ in 0..MAX_BACKGROUND_AGENTS {
            reg.allocate("x".into(), CancellationToken::new(), log_path).unwrap();
        }
        assert!(reg.allocate("overflow".into(), CancellationToken::new(), log_path).is_err());

        // Remove one manually — simulates the spawned task calling remove().
        reg.remove(1);

        // Now there's room for one more.
        assert!(reg.allocate("fits".into(), CancellationToken::new(), log_path).is_ok());
    }

    #[test]
    fn stop_after_remove_errors() {
        let reg = BackgroundAgentRegistry::new();
        let (id, _) = reg.allocate("test".into(), CancellationToken::new(), log_path).unwrap();
        reg.remove(id);
        assert!(reg.stop(id).is_err());
    }
}
