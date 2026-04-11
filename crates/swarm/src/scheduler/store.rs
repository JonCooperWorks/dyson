//! SQLite-backed task store for the scheduler service.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE tasks (
//!   task_id            TEXT PRIMARY KEY,         -- UUIDv7
//!   state              TEXT NOT NULL,            -- TaskState string
//!   skill              TEXT,
//!   prompt             TEXT NOT NULL,
//!   payloads_json      TEXT NOT NULL DEFAULT '[]',
//!   constraints_json   TEXT NOT NULL DEFAULT '{}',
//!   timeout_secs       INTEGER,
//!   assigned_node      TEXT,
//!   submitted_at_ms    INTEGER NOT NULL,
//!   started_at_ms      INTEGER,
//!   finished_at_ms     INTEGER,
//!   last_progress_at_ms INTEGER,
//!   progress_pct       REAL,
//!   progress_message   TEXT,
//!   result_text        TEXT,
//!   result_payloads_json TEXT,
//!   error              TEXT,
//!   duration_secs      INTEGER,
//!   notify_json        TEXT NOT NULL DEFAULT '[]',
//!   notification_delivered INTEGER NOT NULL DEFAULT 0
//! );
//!
//! CREATE TABLE task_logs (
//!   task_id   TEXT NOT NULL,
//!   seq       INTEGER NOT NULL,
//!   ts_ms     INTEGER NOT NULL,
//!   chunk     TEXT NOT NULL,
//!   PRIMARY KEY (task_id, seq)
//! );
//! ```
//!
//! All writes are done on a single tokio blocking thread via
//! [`tokio::task::spawn_blocking`]; rusqlite is sync. The store is
//! `Clone` and uses `Arc<Mutex<Connection>>` internally.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dyson_swarm_protocol::types::Payload;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use tokio::sync::Mutex;

use super::types::{NotifyChannel, RecentResult, TaskRow, TaskState, TerminalStatus};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id              TEXT PRIMARY KEY,
    state                TEXT NOT NULL,
    skill                TEXT,
    prompt               TEXT NOT NULL,
    payloads_json        TEXT NOT NULL DEFAULT '[]',
    constraints_json     TEXT NOT NULL DEFAULT '{}',
    timeout_secs         INTEGER,
    assigned_node        TEXT,
    submitted_at_ms      INTEGER NOT NULL,
    started_at_ms        INTEGER,
    finished_at_ms       INTEGER,
    last_progress_at_ms  INTEGER,
    progress_pct         REAL,
    progress_message     TEXT,
    result_text          TEXT,
    result_payloads_json TEXT,
    error                TEXT,
    duration_secs        INTEGER,
    notify_json          TEXT NOT NULL DEFAULT '[]',
    notification_delivered INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS tasks_by_submitted ON tasks(submitted_at_ms DESC);
CREATE INDEX IF NOT EXISTS tasks_by_state ON tasks(state);

CREATE TABLE IF NOT EXISTS task_logs (
    task_id  TEXT NOT NULL,
    seq      INTEGER NOT NULL,
    ts_ms    INTEGER NOT NULL,
    chunk    TEXT NOT NULL,
    PRIMARY KEY (task_id, seq)
);
"#;

#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, TaskStoreError>;

#[derive(Clone)]
pub struct TaskStore {
    conn: Arc<Mutex<Connection>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms_to_st(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

impl TaskStore {
    /// Open (or create) the store at `path`. Pass `:memory:` for an
    /// in-memory test store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // WAL mode: lets readers proceed during writes.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a brand-new pending task. Returns the assigned task_id
    /// (UUIDv7).
    pub async fn submit(
        &self,
        skill: Option<String>,
        prompt: String,
        payloads: Vec<Payload>,
        timeout_secs: Option<u64>,
        constraints_json: serde_json::Value,
        notify: Vec<NotifyChannel>,
    ) -> Result<String> {
        let task_id = uuid::Uuid::now_v7().to_string();
        let now = now_ms() as i64;
        let payloads_json = serde_json::to_string(&payloads)?;
        let constraints_json = serde_json::to_string(&constraints_json)?;
        let notify_json = serde_json::to_string(&notify)?;
        let task_id_clone = task_id.clone();

        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO tasks
                (task_id, state, skill, prompt, payloads_json, constraints_json, timeout_secs,
                 submitted_at_ms, notify_json)
             VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                task_id_clone,
                skill,
                prompt,
                payloads_json,
                constraints_json,
                timeout_secs.map(|t| t as i64),
                now,
                notify_json,
            ],
        )?;
        Ok(task_id)
    }

    /// Read one task row by id.
    pub async fn get(&self, task_id: &str) -> Result<Option<TaskRow>> {
        let conn = self.conn.lock().await;
        let row = conn
            .query_row(
                "SELECT task_id, state, skill, prompt, assigned_node, submitted_at_ms,
                        started_at_ms, finished_at_ms, last_progress_at_ms, progress_pct,
                        progress_message, result_text, result_payloads_json, error,
                        duration_secs, notify_json, notification_delivered
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                row_to_task,
            )
            .optional()?;
        Ok(row)
    }

    /// Mark a pending task as assigned to a worker. State transitions
    /// `pending → assigned` only.
    pub async fn mark_assigned(&self, task_id: &str, node_id: &str) -> Result<()> {
        let now = now_ms() as i64;
        let conn = self.conn.lock().await;
        let updated = conn.execute(
            "UPDATE tasks SET state = 'assigned', assigned_node = ?2,
                              started_at_ms = COALESCE(started_at_ms, ?3),
                              last_progress_at_ms = ?3
             WHERE task_id = ?1 AND state = 'pending'",
            params![task_id, node_id, now],
        )?;
        if updated == 0 {
            return Err(TaskStoreError::InvalidTransition(format!(
                "task {task_id} is not pending"
            )));
        }
        Ok(())
    }

    /// Record a progress heartbeat from the worker. Allowed in any
    /// non-terminal state; sliding the task back from Stalled to Running.
    pub async fn record_progress(
        &self,
        task_id: &str,
        progress_pct: Option<f32>,
        message: Option<&str>,
        log_chunk: Option<&str>,
    ) -> Result<()> {
        let now = now_ms() as i64;
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;

        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?;
        let state = state.ok_or_else(|| TaskStoreError::NotFound(task_id.to_string()))?;
        let parsed = TaskState::from_str(&state).ok_or_else(|| {
            TaskStoreError::InvalidTransition(format!("unknown stored state: {state}"))
        })?;
        if parsed.is_terminal() {
            return Err(TaskStoreError::InvalidTransition(format!(
                "task {task_id} is already terminal ({state})"
            )));
        }

        // Slide stalled / assigned back to running on a heartbeat.
        let new_state = match parsed {
            TaskState::Cancelling => "cancelling",
            _ => "running",
        };

        tx.execute(
            "UPDATE tasks SET state = ?2, last_progress_at_ms = ?3,
                              progress_pct = COALESCE(?4, progress_pct),
                              progress_message = COALESCE(?5, progress_message)
             WHERE task_id = ?1",
            params![task_id, new_state, now, progress_pct, message],
        )?;

        if let Some(log) = log_chunk {
            let next_seq: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(seq), -1) + 1 FROM task_logs WHERE task_id = ?1",
                    params![task_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            tx.execute(
                "INSERT INTO task_logs (task_id, seq, ts_ms, chunk) VALUES (?1, ?2, ?3, ?4)",
                params![task_id, next_seq, now, log],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Append a log chunk without changing state (for hub-side captures).
    pub async fn append_log(&self, task_id: &str, chunk: &str) -> Result<()> {
        let now = now_ms() as i64;
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let next_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM task_logs WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        tx.execute(
            "INSERT INTO task_logs (task_id, seq, ts_ms, chunk) VALUES (?1, ?2, ?3, ?4)",
            params![task_id, next_seq, now, chunk],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Read recent logs for a task. Returns `(seq, ts_ms, chunk)` rows.
    pub async fn read_logs(
        &self,
        task_id: &str,
        since_seq: i64,
        limit: i64,
    ) -> Result<Vec<(i64, u64, String)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT seq, ts_ms, chunk FROM task_logs
             WHERE task_id = ?1 AND seq > ?2
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![task_id, since_seq, limit], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, String>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a task as terminally finished and store its outcome.
    pub async fn finish(
        &self,
        task_id: &str,
        status: TerminalStatus,
        result_text: Option<String>,
        result_payloads: Option<&[Payload]>,
        duration_secs: u64,
    ) -> Result<()> {
        let now = now_ms() as i64;
        let payloads_json = match result_payloads {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let (state, error) = match &status {
            TerminalStatus::Completed => ("done", None),
            TerminalStatus::Failed { error } => ("failed", Some(error.clone())),
            TerminalStatus::Cancelled { reason } => ("cancelled", reason.clone()),
        };

        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE tasks SET state = ?2, finished_at_ms = ?3, error = ?4,
                              result_text = ?5, result_payloads_json = ?6,
                              duration_secs = ?7
             WHERE task_id = ?1",
            params![task_id, state, now, error, result_text, payloads_json, duration_secs as i64],
        )?;
        Ok(())
    }

    /// Request cancellation. Transitions any non-terminal state to
    /// `cancelling`.
    pub async fn request_cancel(&self, task_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let updated = conn.execute(
            "UPDATE tasks SET state = 'cancelling'
             WHERE task_id = ?1 AND state NOT IN ('done', 'failed', 'cancelled', 'cancelling')",
            params![task_id],
        )?;
        Ok(updated > 0)
    }

    /// Slide non-terminal tasks past `stall_after` from `assigned` /
    /// `running` to `stalled`.
    pub async fn sweep_stalled(&self, stall_after: Duration) -> Result<usize> {
        let cutoff = (now_ms() as i64) - (stall_after.as_millis() as i64);
        let conn = self.conn.lock().await;
        let updated = conn.execute(
            "UPDATE tasks SET state = 'stalled'
             WHERE state IN ('assigned', 'running')
               AND last_progress_at_ms IS NOT NULL
               AND last_progress_at_ms < ?1",
            params![cutoff],
        )?;
        Ok(updated)
    }

    /// Mark notification as delivered (idempotent).
    pub async fn mark_notification_delivered(&self, task_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE tasks SET notification_delivered = 1 WHERE task_id = ?1",
            params![task_id],
        )?;
        Ok(())
    }

    /// List recent terminal tasks for the `swarm_results` MCP tool.
    ///
    /// When `state_filter` is `None`, returns only the three terminal
    /// states (`done`, `failed`, `cancelled`). Pass an explicit filter
    /// to widen the query.
    pub async fn recent_results(
        &self,
        since_ms: Option<u64>,
        limit: i64,
        state_filter: Option<&[&str]>,
    ) -> Result<Vec<RecentResult>> {
        let conn = self.conn.lock().await;
        let mut sql = String::from(
            "SELECT task_id, state, skill, submitted_at_ms, finished_at_ms, duration_secs,
                    assigned_node, result_text, error
             FROM tasks WHERE 1=1",
        );
        if since_ms.is_some() {
            sql.push_str(" AND submitted_at_ms >= ?1");
        }
        let default_terminal = ["done", "failed", "cancelled"];
        let states_to_use: &[&str] = state_filter.unwrap_or(&default_terminal);
        let placeholders = states_to_use
            .iter()
            .map(|_| "?".to_string())
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" AND state IN ({placeholders})"));
        sql.push_str(" ORDER BY submitted_at_ms DESC LIMIT ?");

        // Build params dynamically.
        let mut stmt = conn.prepare(&sql)?;
        let mut bound: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(s) = since_ms {
            bound.push((s as i64).into());
        }
        for s in states_to_use {
            bound.push((*s).to_string().into());
        }
        bound.push(limit.into());

        let rows = stmt.query_map(rusqlite::params_from_iter(bound.iter()), |r| {
            Ok(RecentResult {
                task_id: r.get(0)?,
                state: r.get(1)?,
                skill: r.get(2)?,
                submitted_at_ms: r.get::<_, i64>(3)? as u64,
                finished_at_ms: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                duration_secs: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                assigned_node: r.get(6)?,
                summary: r.get::<_, Option<String>>(7)?.map(|s| {
                    // Truncate to 200 chars for compactness.
                    if s.len() > 200 {
                        format!("{}…", &s[..200])
                    } else {
                        s
                    }
                }),
                error: r.get(8)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Pending tasks waiting for a worker.
    pub async fn pending(&self) -> Result<Vec<TaskRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, state, skill, prompt, assigned_node, submitted_at_ms,
                    started_at_ms, finished_at_ms, last_progress_at_ms, progress_pct,
                    progress_message, result_text, result_payloads_json, error,
                    duration_secs, notify_json, notification_delivered
             FROM tasks WHERE state = 'pending' ORDER BY submitted_at_ms ASC",
        )?;
        let rows = stmt
            .query_map([], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Read the constraints + payloads + timeout for a task by id, used
    /// by the dispatcher when picking a worker.
    pub async fn dispatch_inputs(
        &self,
        task_id: &str,
    ) -> Result<Option<(Vec<Payload>, Option<u64>, serde_json::Value)>> {
        let conn = self.conn.lock().await;
        let row: Option<(String, Option<i64>, String)> = conn
            .query_row(
                "SELECT payloads_json, timeout_secs, constraints_json FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            Some((payloads_json, timeout, constraints_json)) => {
                let payloads = serde_json::from_str(&payloads_json)?;
                let constraints = serde_json::from_str(&constraints_json)?;
                Ok(Some((payloads, timeout.map(|t| t as u64), constraints)))
            }
            None => Ok(None),
        }
    }
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    let state_str: String = r.get(1)?;
    let state = TaskState::from_str(&state_str).unwrap_or(TaskState::Pending);
    let submitted_at = ms_to_st(r.get::<_, i64>(5)? as u64);
    let started_at = r.get::<_, Option<i64>>(6)?.map(|v| ms_to_st(v as u64));
    let finished_at = r.get::<_, Option<i64>>(7)?.map(|v| ms_to_st(v as u64));
    let last_progress_at = r.get::<_, Option<i64>>(8)?.map(|v| ms_to_st(v as u64));
    Ok(TaskRow {
        task_id: r.get(0)?,
        state,
        skill: r.get(2)?,
        prompt: r.get(3)?,
        assigned_node: r.get(4)?,
        submitted_at,
        started_at,
        finished_at,
        last_progress_at,
        progress_pct: r.get(9)?,
        progress_message: r.get(10)?,
        result_text: r.get(11)?,
        result_payloads_json: r.get(12)?,
        error: r.get(13)?,
        duration_secs: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        notify_json: r.get(15)?,
        notification_delivered: r.get::<_, i64>(16)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh() -> TaskStore {
        TaskStore::open_memory().unwrap()
    }

    #[tokio::test]
    async fn submit_then_get() {
        let s = fresh().await;
        let id = s
            .submit(
                Some("bash".into()),
                "do the thing".into(),
                vec![],
                Some(60),
                serde_json::json!({"needs_gpu": false}),
                vec![NotifyChannel::Stdout { template: None }],
            )
            .await
            .unwrap();
        assert_eq!(id.len(), 36); // uuid

        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Pending);
        assert_eq!(row.skill.as_deref(), Some("bash"));
        assert_eq!(row.prompt, "do the thing");
    }

    #[tokio::test]
    async fn assign_transitions_state() {
        let s = fresh().await;
        let id = s
            .submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![])
            .await
            .unwrap();

        s.mark_assigned(&id, "node-1").await.unwrap();
        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Assigned);
        assert_eq!(row.assigned_node.as_deref(), Some("node-1"));
        assert!(row.started_at.is_some());

        // Second assign attempt errors.
        let err = s.mark_assigned(&id, "node-2").await.unwrap_err();
        assert!(matches!(err, TaskStoreError::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn progress_slides_to_running_and_appends_log() {
        let s = fresh().await;
        let id = s.submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        s.mark_assigned(&id, "node-1").await.unwrap();

        s.record_progress(&id, Some(0.5), Some("halfway"), Some("step 1\n"))
            .await
            .unwrap();

        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Running);
        assert_eq!(row.progress_pct, Some(0.5));
        assert_eq!(row.progress_message.as_deref(), Some("halfway"));

        let logs = s.read_logs(&id, -1, 100).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].2, "step 1\n");
    }

    #[tokio::test]
    async fn finish_records_terminal_state() {
        let s = fresh().await;
        let id = s.submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        s.mark_assigned(&id, "n").await.unwrap();

        s.finish(
            &id,
            TerminalStatus::Completed,
            Some("all done".into()),
            None,
            42,
        )
        .await
        .unwrap();

        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Done);
        assert_eq!(row.result_text.as_deref(), Some("all done"));
        assert_eq!(row.duration_secs, Some(42));
        assert!(row.finished_at.is_some());
    }

    #[tokio::test]
    async fn cancel_request_transitions() {
        let s = fresh().await;
        let id = s.submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        s.mark_assigned(&id, "n").await.unwrap();
        assert!(s.request_cancel(&id).await.unwrap());
        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Cancelling);

        // Idempotent: cancelling a cancelling task is a no-op.
        assert!(!s.request_cancel(&id).await.unwrap());
    }

    #[tokio::test]
    async fn sweep_stalled_marks_old_running_tasks() {
        let s = fresh().await;
        let id = s.submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        s.mark_assigned(&id, "n").await.unwrap();
        s.record_progress(&id, None, None, None).await.unwrap();

        // Manually rewind last_progress_at_ms.
        {
            let conn = s.conn.lock().await;
            conn.execute(
                "UPDATE tasks SET last_progress_at_ms = 0 WHERE task_id = ?1",
                params![id],
            )
            .unwrap();
        }

        let n = s.sweep_stalled(Duration::from_secs(1)).await.unwrap();
        assert_eq!(n, 1);
        let row = s.get(&id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Stalled);
    }

    #[tokio::test]
    async fn recent_results_orders_descending() {
        let s = fresh().await;
        let a = s.submit(None, "first".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        // Sleep a tiny bit to ensure ms granularity differs.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let b = s.submit(None, "second".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        s.mark_assigned(&a, "n").await.unwrap();
        s.finish(&a, TerminalStatus::Completed, Some("ok".into()), None, 1).await.unwrap();
        s.mark_assigned(&b, "n").await.unwrap();
        s.finish(&b, TerminalStatus::Failed { error: "boom".into() }, None, None, 1).await.unwrap();

        let rows = s.recent_results(None, 10, None).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].task_id, b);
        assert_eq!(rows[0].state, "failed");
        assert_eq!(rows[1].task_id, a);
        assert_eq!(rows[1].state, "done");
    }

    #[tokio::test]
    async fn dispatch_inputs_roundtrip() {
        let s = fresh().await;
        let id = s
            .submit(
                Some("gpu".into()),
                "fine-tune".into(),
                vec![Payload::Inline {
                    name: "config.yaml".into(),
                    data: b"key: val".to_vec(),
                }],
                Some(3600),
                serde_json::json!({"needs_gpu": true, "min_ram_gb": 32}),
                vec![],
            )
            .await
            .unwrap();
        let (payloads, timeout, constraints) = s.dispatch_inputs(&id).await.unwrap().unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(timeout, Some(3600));
        assert_eq!(constraints["needs_gpu"], true);
        assert_eq!(constraints["min_ram_gb"], 32);
    }

    #[tokio::test]
    async fn notification_delivery_flag() {
        let s = fresh().await;
        let id = s.submit(None, "p".into(), vec![], None, serde_json::json!({}), vec![]).await.unwrap();
        let row = s.get(&id).await.unwrap().unwrap();
        assert!(!row.notification_delivered);
        s.mark_notification_delivered(&id).await.unwrap();
        let row = s.get(&id).await.unwrap().unwrap();
        assert!(row.notification_delivered);
    }
}
