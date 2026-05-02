//! Swarm-mode background state mirror.
//!
//! The normal filesystem workspace stays local and authoritative. This
//! worker runs only from `dyson swarm`: it scans selected durable state
//! files, detects changes, and POSTs the changed bytes to the parent
//! swarm where they are sealed under the owning user's key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;

const SYNC_INTERVAL: Duration = Duration::from_secs(5);
const MAX_SYNC_FILE_BYTES: u64 = 5 * 1024 * 1024;

pub const ENV_STATE_SYNC_URL: &str = "SWARM_STATE_SYNC_URL";
pub const ENV_STATE_SYNC_TOKEN: &str = "SWARM_STATE_SYNC_TOKEN";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateSyncConfig {
    pub url: String,
    pub token: String,
}

#[derive(Clone, Debug)]
struct SyncRoot {
    namespace: &'static str,
    path: PathBuf,
}

static CONFIG: OnceLock<Arc<Mutex<Option<StateSyncConfig>>>> = OnceLock::new();

pub fn config_from_env() -> Option<StateSyncConfig> {
    let url = std::env::var(ENV_STATE_SYNC_URL).unwrap_or_default();
    let token = std::env::var(ENV_STATE_SYNC_TOKEN).unwrap_or_default();
    if url.is_empty() || token.is_empty() {
        None
    } else {
        Some(StateSyncConfig { url, token })
    }
}

pub fn install_config(initial: Option<StateSyncConfig>) -> Arc<Mutex<Option<StateSyncConfig>>> {
    let handle = CONFIG.get_or_init(|| Arc::new(Mutex::new(None))).clone();
    if let Ok(mut guard) = handle.lock() {
        *guard = initial;
    }
    handle
}

pub fn set_config(config: Option<StateSyncConfig>) {
    if let Some(handle) = CONFIG.get()
        && let Ok(mut guard) = handle.lock()
    {
        *guard = config;
    }
}

pub fn spawn_worker(workspace: PathBuf, chats: PathBuf, initial: Option<StateSyncConfig>) {
    let config = install_config(initial);
    let _handle = tokio::spawn(async move {
        let mut worker = StateSyncWorker::new(vec![
            SyncRoot {
                namespace: "workspace",
                path: workspace,
            },
            SyncRoot {
                namespace: "chats",
                path: chats,
            },
        ]);
        let mut last_config: Option<StateSyncConfig> = None;
        loop {
            let current = config.lock().ok().and_then(|guard| guard.clone());
            if let Some(cfg) = current {
                if last_config.as_ref() != Some(&cfg) {
                    worker.clear_sent();
                    last_config = Some(cfg.clone());
                }
                worker.sync_once(&cfg).await;
            } else {
                last_config = None;
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified_ns: u128,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    key: String,
    namespace: &'static str,
    rel_path: String,
    abs_path: PathBuf,
    stamp: FileStamp,
    mime: Option<&'static str>,
}

#[derive(Debug)]
struct StateSyncWorker {
    roots: Vec<SyncRoot>,
    sent: BTreeMap<String, FileStamp>,
}

impl StateSyncWorker {
    fn new(roots: Vec<SyncRoot>) -> Self {
        Self {
            roots,
            sent: BTreeMap::new(),
        }
    }

    fn clear_sent(&mut self) {
        self.sent.clear();
    }

    async fn sync_once(&mut self, config: &StateSyncConfig) {
        let files = collect_files(&self.roots);
        let current_keys: BTreeSet<String> = files.iter().map(|file| file.key.clone()).collect();

        for file in files {
            if self.sent.get(&file.key) == Some(&file.stamp) {
                continue;
            }
            if file.stamp.len > MAX_SYNC_FILE_BYTES {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&file.abs_path).await else {
                continue;
            };
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SYNC_FILE_BYTES {
                continue;
            }
            if post_state_file(config, &file, Some(&bytes), false).await {
                self.sent.insert(file.key, file.stamp);
            }
        }

        let stale: Vec<String> = self
            .sent
            .keys()
            .filter(|key| !current_keys.contains(*key))
            .cloned()
            .collect();
        for key in stale {
            let Some((namespace, rel_path)) = key.split_once(':') else {
                self.sent.remove(&key);
                continue;
            };
            let tombstone = FileSnapshot {
                key: key.clone(),
                namespace: if namespace == "workspace" {
                    "workspace"
                } else {
                    "chats"
                },
                rel_path: rel_path.to_owned(),
                abs_path: PathBuf::new(),
                stamp: FileStamp {
                    len: 0,
                    modified_ns: now_ns(),
                },
                mime: None,
            };
            if post_state_file(config, &tombstone, None, true).await {
                self.sent.remove(&key);
            }
        }
    }
}

#[derive(Serialize)]
struct StateFileUpload<'a> {
    namespace: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<&'a str>,
    updated_at: i64,
    deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_b64: Option<String>,
}

async fn post_state_file(
    config: &StateSyncConfig,
    file: &FileSnapshot,
    body: Option<&[u8]>,
    deleted: bool,
) -> bool {
    let upload = StateFileUpload {
        namespace: file.namespace,
        path: &file.rel_path,
        mime: file.mime,
        updated_at: ns_to_secs(file.stamp.modified_ns),
        deleted,
        body_b64: body.map(|bytes| B64.encode(bytes)),
    };
    match crate::http::client()
        .post(&config.url)
        .bearer_auth(&config.token)
        .json(&upload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::warn!(
                status = %resp.status(),
                namespace = %file.namespace,
                path = %file.rel_path,
                "state-sync: swarm rejected file push"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                namespace = %file.namespace,
                path = %file.rel_path,
                "state-sync: file push failed"
            );
            false
        }
    }
}

fn collect_files(roots: &[SyncRoot]) -> Vec<FileSnapshot> {
    let mut out = Vec::new();
    for root in roots {
        collect_root(root, &root.path, &mut out);
    }
    out
}

fn collect_root(root: &SyncRoot, dir: &Path, out: &mut Vec<FileSnapshot>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_root(root, &path, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(&root.path) else {
            continue;
        };
        if !should_sync(root.namespace, rel) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let rel_path = slash_path(rel);
        let key = format!("{}:{rel_path}", root.namespace);
        let mime = mime_for(rel);
        out.push(FileSnapshot {
            key,
            namespace: root.namespace,
            rel_path,
            abs_path: path,
            stamp: FileStamp {
                len: meta.len(),
                modified_ns: modified_ns(&meta),
            },
            mime,
        });
    }
}

fn should_sync(namespace: &str, rel: &Path) -> bool {
    if has_hidden_or_unclean_component(rel) {
        return false;
    }
    if namespace == "chats" {
        return true;
    }
    if namespace != "workspace" {
        return false;
    }
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [file] => file.ends_with(".md"),
        ["memory", ..] => rel.extension().and_then(|s| s.to_str()) == Some("md"),
        ["kb", ..] | ["skills", ..] => true,
        _ => false,
    }
}

fn has_hidden_or_unclean_component(path: &Path) -> bool {
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let Some(s) = part.to_str() else {
                    return true;
                };
                if s.is_empty() || s.starts_with('.') {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn mime_for(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("md") => Some("text/markdown"),
        Some("json") => Some("application/json"),
        Some("txt") => Some("text/plain"),
        Some("toml") => Some("application/toml"),
        Some("yaml" | "yml") => Some("application/yaml"),
        _ => None,
    }
}

fn modified_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or_else(now_ns)
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn ns_to_secs(ns: u128) -> i64 {
    i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn workspace_allowlist_is_narrow() {
        assert!(should_sync("workspace", Path::new("MEMORY.md")));
        assert!(should_sync("workspace", Path::new("kb/facts.json")));
        assert!(should_sync("workspace", Path::new("skills/a/SKILL.md")));
        assert!(should_sync("workspace", Path::new("skills/a/icon.svg")));
        assert!(!should_sync("workspace", Path::new(".env")));
        assert!(!should_sync("workspace", Path::new("memory.db")));
        assert!(!should_sync("workspace", Path::new("../MEMORY.md")));
    }

    #[test]
    fn chats_allowlist_keeps_clean_chat_tree() {
        assert!(should_sync("chats", Path::new("c-1/transcript.json")));
        assert!(should_sync(
            "chats",
            Path::new("c-1/archives/2026-05-02T10-00-00.json")
        ));
        assert!(should_sync("chats", Path::new("c-1/media/f1.b64")));
        assert!(should_sync("chats", Path::new("c-1/artefacts/a1.body")));
        assert!(should_sync("chats", Path::new("c-1/files/f1.bin")));
        assert!(should_sync("chats", Path::new("c-1/feedback.json")));
        assert!(!should_sync("chats", Path::new(".chats_version")));
        assert!(!should_sync("chats", Path::new("c-1/.tmp")));
        assert!(!should_sync("chats", Path::new("../c-1/transcript.json")));
    }

    #[tokio::test]
    async fn sync_once_posts_changed_workspace_file() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/state"))
            .and(header("authorization", "Bearer st_test"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "hello").unwrap();
        std::fs::write(dir.path().join(".env"), "secret").unwrap();

        let cfg = StateSyncConfig {
            url: format!("{}/state", server.uri()),
            token: "st_test".into(),
        };
        let mut worker = StateSyncWorker::new(vec![SyncRoot {
            namespace: "workspace",
            path: dir.path().to_path_buf(),
        }]);
        worker.sync_once(&cfg).await;
        worker.sync_once(&cfg).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["namespace"], "workspace");
        assert_eq!(body["path"], "MEMORY.md");
        assert_eq!(body["mime"], "text/markdown");
        assert_eq!(
            B64.decode(body["body_b64"].as_str().unwrap()).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn sync_once_posts_tombstone_after_delete() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/state"))
            .respond_with(ResponseTemplate::new(204))
            .expect(2)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MEMORY.md");
        std::fs::write(&path, "hello").unwrap();

        let cfg = StateSyncConfig {
            url: format!("{}/state", server.uri()),
            token: "st_test".into(),
        };
        let mut worker = StateSyncWorker::new(vec![SyncRoot {
            namespace: "workspace",
            path: dir.path().to_path_buf(),
        }]);
        worker.sync_once(&cfg).await;
        std::fs::remove_file(path).unwrap();
        worker.sync_once(&cfg).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let tombstone: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(tombstone["path"], "MEMORY.md");
        assert_eq!(tombstone["deleted"], true);
        assert!(tombstone.get("body_b64").is_none());
    }
}
