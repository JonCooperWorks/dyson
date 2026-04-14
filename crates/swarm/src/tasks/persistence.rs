//! Durable persistence layer for the task store.
//!
//! Provides a `TaskPersistence` trait and a SQLite implementation via sqlx.
//! Every mutation in `TaskStore` writes through to the persistence layer
//! so tasks survive hub restarts.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use dyson_swarm_protocol::types::{SwarmResult, TaskCheckpoint};

use super::{TaskRecord, TaskState};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Async persistence backend for the task store.
///
/// All methods are async because sqlx is async-native.  The `TaskStore`
/// calls these *outside* the in-memory write lock so readers are never
/// blocked by I/O.
#[async_trait]
pub trait TaskPersistence: Send + Sync {
    async fn insert(&self, record: &TaskRecord) -> Result<(), sqlx::Error>;
    async fn append_checkpoint(&self, task_id: &str, cp: &TaskCheckpoint)
        -> Result<(), sqlx::Error>;
    async fn finalize(
        &self,
        task_id: &str,
        result: &SwarmResult,
        state: &TaskState,
    ) -> Result<(), sqlx::Error>;
    async fn cancel(&self, task_id: &str, result: &SwarmResult) -> Result<(), sqlx::Error>;
    async fn remove(&self, task_id: &str) -> Result<(), sqlx::Error>;
    /// Called once on startup — returns every persisted task with its
    /// checkpoints attached.
    async fn load_all(&self) -> Result<Vec<TaskRecord>, sqlx::Error>;
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id           TEXT PRIMARY KEY,
    node_id           TEXT NOT NULL,
    owner             TEXT,
    prompt_preview    TEXT NOT NULL,
    submitted_at_unix INTEGER NOT NULL,
    last_update_unix  INTEGER NOT NULL,
    state             TEXT NOT NULL,
    error             TEXT,
    result_json       TEXT
);

CREATE INDEX IF NOT EXISTS idx_tasks_state  ON tasks(state);
CREATE INDEX IF NOT EXISTS idx_tasks_owner  ON tasks(owner);
CREATE INDEX IF NOT EXISTS idx_tasks_submit ON tasks(submitted_at_unix DESC);

CREATE TABLE IF NOT EXISTS task_checkpoints (
    task_id         TEXT NOT NULL,
    sequence        INTEGER NOT NULL,
    message         TEXT NOT NULL,
    progress        REAL,
    emitted_at_secs INTEGER NOT NULL,
    PRIMARY KEY (task_id, sequence),
    FOREIGN KEY (task_id) REFERENCES tasks(task_id) ON DELETE CASCADE
);
"#;

/// SQLite-backed persistence using sqlx.
pub struct SqliteTaskPersistence {
    pool: SqlitePool,
}

impl SqliteTaskPersistence {
    /// Open (or create) a SQLite database at the given path.
    pub async fn open(path: &std::path::Path) -> Result<Self, sqlx::Error> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        sqlx::raw_sql(SCHEMA).execute(&pool).await?;

        tracing::info!(path = %path.display(), "task persistence store opened");
        Ok(Self { pool })
    }

    /// Open an in-memory database (for tests).
    pub async fn open_in_memory() -> Result<Self, sqlx::Error> {
        // Use a shared in-memory DB so the pool's multiple connections
        // all see the same data.
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .shared_cache(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;

        sqlx::raw_sql(SCHEMA).execute(&pool).await?;

        Ok(Self { pool })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const fn state_to_str(state: &TaskState) -> &'static str {
    match state {
        TaskState::Running => "running",
        TaskState::Completed => "completed",
        TaskState::Failed { .. } => "failed",
        TaskState::Cancelled => "cancelled",
    }
}

const fn state_error(state: &TaskState) -> Option<&str> {
    match state {
        TaskState::Failed { error } => Some(error.as_str()),
        _ => None,
    }
}

fn parse_state(state: &str, error: Option<String>) -> TaskState {
    match state {
        "running" => TaskState::Running,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed {
            error: error.unwrap_or_default(),
        },
        "cancelled" => TaskState::Cancelled,
        other => {
            tracing::warn!(state = other, "unknown task state in DB, treating as failed");
            TaskState::Failed {
                error: format!("unknown state: {other}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TaskPersistence impl
// ---------------------------------------------------------------------------

#[async_trait]
impl TaskPersistence for SqliteTaskPersistence {
    async fn insert(&self, record: &TaskRecord) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR REPLACE INTO tasks \
             (task_id, node_id, owner, prompt_preview, submitted_at_unix, \
              last_update_unix, state, error, result_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(&record.task_id)
        .bind(&record.node_id)
        .bind(&record.owner)
        .bind(&record.prompt_preview)
        .bind(unix_secs(record.submitted_at))
        .bind(unix_secs(record.last_update))
        .bind(state_to_str(&record.state))
        .bind(state_error(&record.state))
        .bind(
            record
                .result
                .as_ref()
                .and_then(|r| serde_json::to_string(r).ok()),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn append_checkpoint(
        &self,
        _task_id: &str,
        cp: &TaskCheckpoint,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR REPLACE INTO task_checkpoints \
             (task_id, sequence, message, progress, emitted_at_secs) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&cp.task_id)
        .bind(cp.sequence as i64)
        .bind(&cp.message)
        .bind(cp.progress.map(|p| p as f64))
        .bind(cp.emitted_at_secs as i64)
        .execute(&self.pool)
        .await?;

        // Also bump last_update_unix on the parent task row.
        sqlx::query("UPDATE tasks SET last_update_unix = ?1 WHERE task_id = ?2")
            .bind(unix_secs(SystemTime::now()))
            .bind(&cp.task_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn finalize(
        &self,
        task_id: &str,
        result: &SwarmResult,
        state: &TaskState,
    ) -> Result<(), sqlx::Error> {
        let result_json = serde_json::to_string(result).ok();
        sqlx::query(
            "UPDATE tasks SET state = ?1, error = ?2, result_json = ?3, \
             last_update_unix = ?4 WHERE task_id = ?5",
        )
        .bind(state_to_str(state))
        .bind(state_error(state))
        .bind(&result_json)
        .bind(unix_secs(SystemTime::now()))
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn cancel(&self, task_id: &str, result: &SwarmResult) -> Result<(), sqlx::Error> {
        let result_json = serde_json::to_string(result).ok();
        sqlx::query(
            "UPDATE tasks SET state = 'cancelled', result_json = ?1, \
             last_update_unix = ?2 WHERE task_id = ?3",
        )
        .bind(&result_json)
        .bind(unix_secs(SystemTime::now()))
        .bind(task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove(&self, task_id: &str) -> Result<(), sqlx::Error> {
        // Checkpoints cascade-delete via FK.
        sqlx::query("DELETE FROM tasks WHERE task_id = ?1")
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn load_all(&self) -> Result<Vec<TaskRecord>, sqlx::Error> {
        // Load all task rows.
        let task_rows = sqlx::query(
            "SELECT task_id, node_id, owner, prompt_preview, \
             submitted_at_unix, last_update_unix, state, error, result_json \
             FROM tasks ORDER BY submitted_at_unix DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        // Load all checkpoints, grouped by task.
        let cp_rows = sqlx::query(
            "SELECT task_id, sequence, message, progress, emitted_at_secs \
             FROM task_checkpoints ORDER BY task_id, sequence",
        )
        .fetch_all(&self.pool)
        .await?;

        // Group checkpoints by task_id.
        let mut cp_map: std::collections::HashMap<String, Vec<TaskCheckpoint>> =
            std::collections::HashMap::new();
        for row in &cp_rows {
            let task_id: String = row.get("task_id");
            let cp = TaskCheckpoint {
                task_id: task_id.clone(),
                sequence: row.get::<i64, _>("sequence") as u32,
                message: row.get("message"),
                progress: row.get::<Option<f64>, _>("progress").map(|p| p as f32),
                emitted_at_secs: row.get::<i64, _>("emitted_at_secs") as u64,
            };
            cp_map.entry(task_id).or_default().push(cp);
        }

        // Build TaskRecords.
        let mut records = Vec::with_capacity(task_rows.len());
        for row in &task_rows {
            let task_id: String = row.get("task_id");
            let state_str: String = row.get("state");
            let error: Option<String> = row.get("error");
            let result_json: Option<String> = row.get("result_json");

            let submitted_at_unix: i64 = row.get("submitted_at_unix");
            let last_update_unix: i64 = row.get("last_update_unix");

            let record = TaskRecord {
                task_id: task_id.clone(),
                node_id: row.get("node_id"),
                owner: row.get("owner"),
                prompt_preview: row.get("prompt_preview"),
                submitted_at: UNIX_EPOCH
                    + std::time::Duration::from_secs(submitted_at_unix as u64),
                last_update: UNIX_EPOCH
                    + std::time::Duration::from_secs(last_update_unix as u64),
                state: parse_state(&state_str, error),
                checkpoints: cp_map.remove(&task_id).unwrap_or_default(),
                result: result_json.and_then(|j| serde_json::from_str(&j).ok()),
                waiter: None, // Cannot survive a restart.
            };
            records.push(record);
        }

        Ok(records)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dyson_swarm_protocol::types::TaskStatus;

    fn sample_record(task_id: &str) -> TaskRecord {
        TaskRecord {
            task_id: task_id.into(),
            node_id: "node-a".into(),
            owner: Some("alice".into()),
            prompt_preview: "do a thing".into(),
            submitted_at: SystemTime::now(),
            last_update: SystemTime::now(),
            state: TaskState::Running,
            checkpoints: Vec::new(),
            result: None,
            waiter: None,
        }
    }

    fn sample_result(task_id: &str, status: TaskStatus) -> SwarmResult {
        SwarmResult {
            task_id: task_id.into(),
            text: "ok".into(),
            payloads: vec![],
            status,
            duration_secs: 1,
        }
    }

    #[tokio::test]
    async fn sqlite_insert_then_load_all_roundtrip() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        let rec = sample_record("t1");
        store.insert(&rec).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "t1");
        assert_eq!(loaded[0].node_id, "node-a");
        assert_eq!(loaded[0].owner.as_deref(), Some("alice"));
        assert!(matches!(loaded[0].state, TaskState::Running));
        assert!(loaded[0].waiter.is_none());
    }

    #[tokio::test]
    async fn sqlite_append_checkpoint_appears_in_load_all() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        store.insert(&sample_record("t1")).await.unwrap();

        for seq in 1..=3 {
            store
                .append_checkpoint(
                    "t1",
                    &TaskCheckpoint {
                        task_id: "t1".into(),
                        sequence: seq,
                        message: format!("step {seq}"),
                        progress: Some(seq as f32 / 3.0),
                        emitted_at_secs: seq as u64,
                    },
                )
                .await
                .unwrap();
        }

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded[0].checkpoints.len(), 3);
        assert_eq!(loaded[0].checkpoints[0].sequence, 1);
        assert_eq!(loaded[0].checkpoints[2].sequence, 3);
        assert!(loaded[0].checkpoints[0].progress.is_some());
    }

    #[tokio::test]
    async fn sqlite_finalize_stores_result_and_state() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        store.insert(&sample_record("t1")).await.unwrap();

        let result = sample_result("t1", TaskStatus::Completed);
        store
            .finalize("t1", &result, &TaskState::Completed)
            .await
            .unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(matches!(loaded[0].state, TaskState::Completed));
        let r = loaded[0].result.as_ref().unwrap();
        assert_eq!(r.text, "ok");
        assert!(matches!(r.status, TaskStatus::Completed));
    }

    #[tokio::test]
    async fn sqlite_cancel_marks_cancelled() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        store.insert(&sample_record("t1")).await.unwrap();

        let result = sample_result("t1", TaskStatus::Cancelled);
        store.cancel("t1", &result).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert!(matches!(loaded[0].state, TaskState::Cancelled));
        assert!(loaded[0].result.is_some());
    }

    #[tokio::test]
    async fn sqlite_handles_failed_state_with_error() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        store.insert(&sample_record("t1")).await.unwrap();

        let result = sample_result(
            "t1",
            TaskStatus::Failed {
                error: "boom".into(),
            },
        );
        let state = TaskState::Failed {
            error: "boom".into(),
        };
        store.finalize("t1", &result, &state).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        match &loaded[0].state {
            TaskState::Failed { error } => assert_eq!(error, "boom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sqlite_remove_deletes_row() {
        let store = SqliteTaskPersistence::open_in_memory().await.unwrap();
        store.insert(&sample_record("t1")).await.unwrap();
        store
            .append_checkpoint(
                "t1",
                &TaskCheckpoint {
                    task_id: "t1".into(),
                    sequence: 1,
                    message: "hi".into(),
                    progress: None,
                    emitted_at_secs: 0,
                },
            )
            .await
            .unwrap();

        store.remove("t1").await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert!(loaded.is_empty());
    }
}
