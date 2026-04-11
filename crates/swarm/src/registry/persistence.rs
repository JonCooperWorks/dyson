//! Durable persistence layer for the node registry.
//!
//! Provides a `NodePersistence` trait and a SQLite implementation via sqlx.
//! Every mutation in `NodeRegistry` writes through to the persistence layer
//! so the node roster survives hub restarts.
//!
//! Only persistable state is written: the bearer token, the manifest, the
//! latest `NodeStatus`, and a wall-clock heartbeat timestamp.  The live
//! `sse_tx` channel and the monotonic `Instant` cannot survive a restart
//! and are recreated on recovery.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use dyson_swarm_protocol::types::{NodeManifest, NodeStatus};

// ---------------------------------------------------------------------------
// PersistedNode — subset of NodeEntry that is safe to write to disk
// ---------------------------------------------------------------------------

/// On-disk projection of a `NodeEntry`.
///
/// Strips the runtime-only fields (`sse_tx`, monotonic `last_heartbeat`) and
/// keeps only what can meaningfully survive a restart.
#[derive(Debug, Clone)]
pub struct PersistedNode {
    pub node_id: String,
    pub token: String,
    pub manifest: NodeManifest,
    pub status: NodeStatus,
    pub last_heartbeat_at: SystemTime,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Async persistence backend for the node registry.
///
/// `NodeRegistry` calls these *outside* its write lock so readers are never
/// blocked by I/O.
#[async_trait]
pub trait NodePersistence: Send + Sync {
    async fn insert(&self, entry: &PersistedNode) -> Result<(), sqlx::Error>;
    async fn update_status(
        &self,
        node_id: &str,
        status: &NodeStatus,
        last_heartbeat_unix: i64,
    ) -> Result<(), sqlx::Error>;
    async fn remove(&self, node_id: &str) -> Result<(), sqlx::Error>;
    /// Called once on startup — returns every persisted node row.
    async fn load_all(&self) -> Result<Vec<PersistedNode>, sqlx::Error>;
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    node_id              TEXT PRIMARY KEY,
    token                TEXT NOT NULL,
    manifest_json        TEXT NOT NULL,
    status_json          TEXT NOT NULL,
    last_heartbeat_unix  INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_token ON nodes(token);
"#;

/// SQLite-backed persistence using sqlx.
pub struct SqliteNodePersistence {
    pool: SqlitePool,
}

impl SqliteNodePersistence {
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

        tracing::info!(path = %path.display(), "node persistence store opened");
        Ok(Self { pool })
    }

    /// Open an in-memory database (for tests).
    pub async fn open_in_memory() -> Result<Self, sqlx::Error> {
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

// ---------------------------------------------------------------------------
// NodePersistence impl
// ---------------------------------------------------------------------------

#[async_trait]
impl NodePersistence for SqliteNodePersistence {
    async fn insert(&self, entry: &PersistedNode) -> Result<(), sqlx::Error> {
        let manifest_json = serde_json::to_string(&entry.manifest)
            .map_err(|e| sqlx::Error::Protocol(format!("manifest serialize: {e}")))?;
        let status_json = serde_json::to_string(&entry.status)
            .map_err(|e| sqlx::Error::Protocol(format!("status serialize: {e}")))?;

        sqlx::query(
            "INSERT OR REPLACE INTO nodes \
             (node_id, token, manifest_json, status_json, last_heartbeat_unix) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&entry.node_id)
        .bind(&entry.token)
        .bind(&manifest_json)
        .bind(&status_json)
        .bind(unix_secs(entry.last_heartbeat_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_status(
        &self,
        node_id: &str,
        status: &NodeStatus,
        last_heartbeat_unix: i64,
    ) -> Result<(), sqlx::Error> {
        let status_json = serde_json::to_string(status)
            .map_err(|e| sqlx::Error::Protocol(format!("status serialize: {e}")))?;

        sqlx::query(
            "UPDATE nodes SET status_json = ?1, last_heartbeat_unix = ?2 \
             WHERE node_id = ?3",
        )
        .bind(&status_json)
        .bind(last_heartbeat_unix)
        .bind(node_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove(&self, node_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM nodes WHERE node_id = ?1")
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn load_all(&self) -> Result<Vec<PersistedNode>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT node_id, token, manifest_json, status_json, last_heartbeat_unix \
             FROM nodes ORDER BY node_id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let node_id: String = row.get("node_id");
            let token: String = row.get("token");
            let manifest_json: String = row.get("manifest_json");
            let status_json: String = row.get("status_json");
            let last_heartbeat_unix: i64 = row.get("last_heartbeat_unix");

            let manifest: NodeManifest = match serde_json::from_str(&manifest_json) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(node_id = %node_id, error = %e, "skipping node with unparseable manifest");
                    continue;
                }
            };
            let status: NodeStatus = match serde_json::from_str(&status_json) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(node_id = %node_id, error = %e, "skipping node with unparseable status");
                    continue;
                }
            };

            out.push(PersistedNode {
                node_id,
                token,
                manifest,
                status,
                last_heartbeat_at: UNIX_EPOCH
                    + std::time::Duration::from_secs(last_heartbeat_unix.max(0) as u64),
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dyson_swarm_protocol::types::{HardwareInfo, NodeManifest, NodeStatus};

    fn sample_node(node_id: &str, token: &str) -> PersistedNode {
        PersistedNode {
            node_id: node_id.into(),
            token: token.into(),
            manifest: NodeManifest {
                node_name: node_id.into(),
                os: "linux".into(),
                hardware: HardwareInfo {
                    cpus: vec![],
                    gpus: vec![],
                    ram_bytes: 16 * 1024 * 1024 * 1024,
                    disk_free_bytes: 0,
                },
                capabilities: vec!["bash".into()],
                status: NodeStatus::Idle,
            },
            status: NodeStatus::Idle,
            last_heartbeat_at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn sqlite_insert_then_load_all_roundtrip() {
        let store = SqliteNodePersistence::open_in_memory().await.unwrap();
        let node = sample_node("n1", "tok-1");
        store.insert(&node).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].node_id, "n1");
        assert_eq!(loaded[0].token, "tok-1");
        assert_eq!(loaded[0].manifest.node_name, "n1");
        assert!(matches!(loaded[0].status, NodeStatus::Idle));
    }

    #[tokio::test]
    async fn sqlite_update_status_rewrites_status_and_heartbeat() {
        let store = SqliteNodePersistence::open_in_memory().await.unwrap();
        store.insert(&sample_node("n1", "tok-1")).await.unwrap();

        let new_status = NodeStatus::Busy {
            task_id: "t-7".into(),
        };
        store
            .update_status("n1", &new_status, 1_700_000_000)
            .await
            .unwrap();

        let loaded = store.load_all().await.unwrap();
        match &loaded[0].status {
            NodeStatus::Busy { task_id } => assert_eq!(task_id, "t-7"),
            other => panic!("expected Busy, got {other:?}"),
        }
        assert_eq!(
            unix_secs(loaded[0].last_heartbeat_at),
            1_700_000_000,
            "heartbeat should have been durably updated"
        );
    }

    #[tokio::test]
    async fn sqlite_remove_deletes_row() {
        let store = SqliteNodePersistence::open_in_memory().await.unwrap();
        store.insert(&sample_node("n1", "tok-1")).await.unwrap();
        store.remove("n1").await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn sqlite_insert_or_replace_updates_existing_row() {
        let store = SqliteNodePersistence::open_in_memory().await.unwrap();
        store.insert(&sample_node("n1", "tok-1")).await.unwrap();

        let mut updated = sample_node("n1", "tok-1");
        updated.manifest.capabilities = vec!["bash".into(), "web_search".into()];
        store.insert(&updated).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].manifest.capabilities.len(), 2);
    }
}
