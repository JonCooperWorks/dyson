//! Swarm-mode background state mirror.
//!
//! In swarm mode the local filesystem is the live hot-cache projection
//! the agent edits, while swarm is the durable authority. This worker
//! scans selected durable state files, detects changes, and POSTs changed
//! bytes to the parent swarm where they are sealed under the owning user's
//! key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use dyson_common::state_sync::{ENV_STATE_SYNC_TOKEN, ENV_STATE_SYNC_URL};
use serde::Serialize;

const SYNC_INTERVAL: Duration = Duration::from_secs(5);
const MAX_SYNC_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Ceiling for the per-file transient-failure backoff so a file that hit a
/// long outage still gets retried a couple of times per hour.
const MAX_TRANSIENT_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Per-file exponential backoff after `failures` consecutive transient
/// (non-4xx) push failures: 2·interval, 4·interval, … capped.
fn transient_backoff(failures: u32) -> Duration {
    let factor = 2u32.saturating_pow(failures.min(16));
    (SYNC_INTERVAL * factor).min(MAX_TRANSIENT_BACKOFF)
}

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
static STATUS: OnceLock<Arc<Mutex<StateSyncStatus>>> = OnceLock::new();

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct StateSyncStatus {
    pub configured: bool,
    pub last_success_at: Option<i64>,
    pub last_error_at: Option<i64>,
    pub last_error: Option<String>,
}

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
    let handle = config_handle();
    let configured = initial.is_some();
    if let Ok(mut guard) = handle.lock() {
        *guard = initial;
    }
    update_configured_status(configured);
    handle
}

pub fn set_config(config: Option<StateSyncConfig>) {
    let configured = config.is_some();
    let handle = config_handle();
    if let Ok(mut guard) = handle.lock() {
        *guard = config;
    }
    update_configured_status(configured);
}

pub fn status_snapshot() -> StateSyncStatus {
    status_handle()
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

pub fn config_snapshot() -> Option<StateSyncConfig> {
    config_handle().lock().ok().and_then(|guard| guard.clone())
}

fn config_handle() -> Arc<Mutex<Option<StateSyncConfig>>> {
    CONFIG.get_or_init(|| Arc::new(Mutex::new(None))).clone()
}

fn status_handle() -> Arc<Mutex<StateSyncStatus>> {
    STATUS
        .get_or_init(|| Arc::new(Mutex::new(StateSyncStatus::default())))
        .clone()
}

fn update_configured_status(configured: bool) {
    let status = status_handle();
    if let Ok(mut guard) = status.lock() {
        guard.configured = configured;
        if !configured {
            guard.last_error = None;
            guard.last_error_at = None;
        }
    }
}

fn record_success() {
    let status = status_handle();
    if let Ok(mut guard) = status.lock() {
        guard.configured = true;
        guard.last_success_at = Some(now_secs());
        guard.last_error = None;
        guard.last_error_at = None;
    }
}

fn record_error(error: impl Into<String>) {
    let status = status_handle();
    if let Ok(mut guard) = status.lock() {
        guard.configured = true;
        guard.last_error_at = Some(now_secs());
        guard.last_error = Some(error.into());
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

/// Per-file retry bookkeeping so a failing push can't spin at the sync
/// interval forever.
///
/// - Deterministic 4xx rejections park the file until its stamp (len +
///   mtime) actually changes — re-POSTing identical bytes would produce
///   the identical rejection.
/// - Transient failures (5xx, transport) back off exponentially per file,
///   capped at [`MAX_TRANSIENT_BACKOFF`].
#[derive(Debug, Clone)]
struct RetryState {
    /// Stamp the swarm deterministically rejected — skip until it changes.
    rejected_stamp: Option<FileStamp>,
    /// Consecutive transient failures.
    failures: u32,
    /// Earliest instant of the next transient retry attempt.
    next_attempt: std::time::Instant,
}

/// Classification of one push attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushOutcome {
    Accepted,
    /// Deterministic client-side rejection (4xx other than 408/429):
    /// retrying the same bytes cannot succeed.
    Rejected,
    /// Transient failure (5xx, 408/429, transport error): retry with backoff.
    Transient,
}

#[derive(Debug)]
struct StateSyncWorker {
    roots: Vec<SyncRoot>,
    sent: BTreeMap<String, FileStamp>,
    retry: BTreeMap<String, RetryState>,
}

impl StateSyncWorker {
    fn new(roots: Vec<SyncRoot>) -> Self {
        Self {
            roots,
            sent: BTreeMap::new(),
            retry: BTreeMap::new(),
        }
    }

    fn clear_sent(&mut self) {
        self.sent.clear();
        self.retry.clear();
    }

    /// Record a push outcome for `key`, returning `true` when accepted.
    fn note_outcome(&mut self, key: &str, stamp: FileStamp, outcome: PushOutcome) -> bool {
        match outcome {
            PushOutcome::Accepted => {
                self.retry.remove(key);
                true
            }
            PushOutcome::Rejected => {
                self.retry.insert(
                    key.to_owned(),
                    RetryState {
                        rejected_stamp: Some(stamp),
                        failures: 0,
                        next_attempt: std::time::Instant::now(),
                    },
                );
                false
            }
            PushOutcome::Transient => {
                let failures = self
                    .retry
                    .get(key)
                    .map_or(0, |s| s.failures)
                    .saturating_add(1);
                self.retry.insert(
                    key.to_owned(),
                    RetryState {
                        rejected_stamp: None,
                        failures,
                        next_attempt: std::time::Instant::now() + transient_backoff(failures),
                    },
                );
                false
            }
        }
    }

    /// Whether `key` at `stamp` is currently eligible for a push attempt.
    fn eligible(&self, key: &str, stamp: Option<FileStamp>) -> bool {
        match self.retry.get(key) {
            None => true,
            Some(state) => {
                if let (Some(rejected), Some(stamp)) = (state.rejected_stamp, stamp) {
                    // Parked on a deterministic rejection: only a content
                    // change makes a retry worthwhile.
                    return rejected != stamp;
                }
                if state.rejected_stamp.is_some() {
                    // Rejected tombstone-shaped entry with no new stamp.
                    return false;
                }
                std::time::Instant::now() >= state.next_attempt
            }
        }
    }

    async fn sync_once(&mut self, config: &StateSyncConfig) {
        // The recursive directory walk is synchronous fs work — run it on
        // the blocking pool so a big tree doesn't stall the async runtime
        // every 5 seconds.
        let roots = self.roots.clone();
        let files = match tokio::task::spawn_blocking(move || collect_files(&roots)).await {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!(error = %e, "state-sync: file walk task failed");
                return;
            }
        };
        let current_keys: BTreeSet<String> = files.iter().map(|file| file.key.clone()).collect();

        for file in files {
            if self.sent.get(&file.key) == Some(&file.stamp) {
                self.retry.remove(&file.key);
                continue;
            }
            if file.stamp.len > MAX_SYNC_FILE_BYTES {
                continue;
            }
            if !self.eligible(&file.key, Some(file.stamp)) {
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&file.abs_path).await else {
                continue;
            };
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SYNC_FILE_BYTES {
                continue;
            }
            let outcome = post_state_file(config, &file, Some(&bytes), false).await;
            if self.note_outcome(&file.key, file.stamp, outcome) {
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
            if !self.eligible(&key, None) {
                continue;
            }
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
            match post_state_file(config, &tombstone, None, true).await {
                PushOutcome::Accepted => {
                    self.sent.remove(&key);
                    self.retry.remove(&key);
                }
                // Only content/path-shape rejections are permanent.
                // Auth, conflict, or server-policy 4xx responses can be
                // transient across resume/reconfigure, so keep tombstones
                // pending unless the swarm accepts them or says the remote
                // file is already absent.
                PushOutcome::Rejected => {
                    self.sent.remove(&key);
                    self.retry.remove(&key);
                }
                PushOutcome::Transient => {
                    let _ = self.note_outcome(&key, tombstone.stamp, PushOutcome::Transient);
                }
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
) -> PushOutcome {
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
        Ok(resp) if resp.status().is_success() => {
            record_success();
            PushOutcome::Accepted
        }
        Ok(resp) => {
            let status = resp.status();
            let error = format!("swarm rejected file push: {status}");
            tracing::warn!(
                status = %status,
                namespace = %file.namespace,
                path = %file.rel_path,
                "state-sync: swarm rejected file push"
            );
            record_error(error);
            if deleted
                && matches!(
                    status,
                    reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
                )
            {
                PushOutcome::Accepted
            } else if is_permanent_state_file_rejection(status) {
                PushOutcome::Rejected
            } else {
                PushOutcome::Transient
            }
        }
        Err(e) => {
            let error = e.to_string();
            tracing::warn!(
                error = %e,
                namespace = %file.namespace,
                path = %file.rel_path,
                "state-sync: file push failed"
            );
            record_error(error);
            PushOutcome::Transient
        }
    }
}

fn is_permanent_state_file_rejection(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::PAYLOAD_TOO_LARGE
            | reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
            | reqwest::StatusCode::UNPROCESSABLE_ENTITY
    )
}

fn collect_files(roots: &[SyncRoot]) -> Vec<FileSnapshot> {
    let mut out = Vec::new();
    for root in roots {
        collect_root(root, &root.path, &mut out);
    }
    out
}

/// Test-only probe recording which directories the walk actually enters,
/// so the prune behaviour is assertable (traversal has no other observable
/// side effect).
#[cfg(test)]
pub(crate) mod walk_probe {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static VISITED: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };
    }

    pub(crate) fn start() {
        VISITED.with(|v| *v.borrow_mut() = Some(Vec::new()));
    }

    pub(crate) fn record(dir: &Path) {
        VISITED.with(|v| {
            if let Some(list) = v.borrow_mut().as_mut() {
                list.push(dir.to_path_buf());
            }
        });
    }

    pub(crate) fn take() -> Vec<PathBuf> {
        VISITED.with(|v| v.borrow_mut().take().unwrap_or_default())
    }
}

fn collect_root(root: &SyncRoot, dir: &Path, out: &mut Vec<FileSnapshot>) {
    #[cfg(test)]
    walk_probe::record(dir);
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
            // Prune non-syncable subtrees instead of walking them:
            // `should_sync` rejects any path with a hidden/unclean
            // component, so nothing under such a directory can ever
            // sync — and this walk runs every 5 seconds (a workspace
            // `.git` tree alone is tens of thousands of entries).
            let Ok(rel) = path.strip_prefix(&root.path) else {
                continue;
            };
            if should_descend(root.namespace, rel) {
                collect_root(root, &path, out);
            }
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
        if is_zero_byte_chat_transcript(root.namespace, &rel_path, meta.len()) {
            continue;
        }
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

/// Whether the walk should recurse into this directory.  The workspace
/// allowlist is path-shaped, so prune visible but unsyncable trees like
/// `target/` and `node_modules/` instead of re-walking them every poll.
fn should_descend(namespace: &str, rel: &Path) -> bool {
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
    matches!(
        parts.as_slice(),
        ["memory", ..]
            | ["kb", ..]
            | ["skills", ..]
            | ["channels"]
            | ["channels", _]
            | ["channels", _, "memory", ..]
    )
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
        [file] => has_extension(file, "md"),
        ["memory", ..] => rel
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md")),
        ["kb", ..] | ["skills", ..] => true,
        ["channels", _channel, rest @ ..] => should_sync_channel_workspace(rest, rel),
        _ => false,
    }
}

pub(crate) fn is_durable_state_file_path(namespace: &str, rel_path: &str) -> bool {
    should_sync(namespace, Path::new(rel_path))
}

fn should_sync_channel_workspace(parts: &[&str], rel: &Path) -> bool {
    match parts {
        [file] => has_extension(file, "md") || *file == "_audit.jsonl",
        ["memory", ..] => rel
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md")),
        _ => false,
    }
}

fn has_extension(file_name: &str, expected: &str) -> bool {
    Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

pub(crate) fn is_zero_byte_chat_transcript(namespace: &str, rel_path: &str, len: u64) -> bool {
    namespace == "chats" && len == 0 && rel_path.ends_with("/transcript.json")
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
        Some("jsonl") => Some("application/x-ndjson"),
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
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
    fn set_config_initializes_global_handle() {
        let cfg = StateSyncConfig {
            url: format!(
                "https://swarm.test{}",
                dyson_common::contracts::SWARM_INTERNAL_STATE_FILE_PATH
            ),
            token: "st_test".into(),
        };

        set_config(Some(cfg.clone()));
        let current = CONFIG
            .get()
            .and_then(|handle| handle.lock().ok().and_then(|guard| guard.clone()));

        assert_eq!(current, Some(cfg));
        assert!(status_snapshot().configured);
    }

    #[test]
    fn workspace_allowlist_is_narrow() {
        assert!(should_sync("workspace", Path::new("MEMORY.md")));
        assert!(should_sync("workspace", Path::new("NOTES.MD")));
        assert!(should_sync("workspace", Path::new("kb/facts.json")));
        assert!(should_sync("workspace", Path::new("skills/a/SKILL.md")));
        assert!(should_sync("workspace", Path::new("skills/a/icon.svg")));
        assert!(should_sync(
            "workspace",
            Path::new("channels/group-1/MEMORY.md")
        ));
        assert!(should_sync(
            "workspace",
            Path::new("channels/group-1/memory/2026-05-09.md")
        ));
        assert!(should_sync(
            "workspace",
            Path::new("channels/group-1/_audit.jsonl")
        ));
        assert!(!should_sync("workspace", Path::new(".env")));
        assert!(!should_sync("workspace", Path::new("dyson.json")));
        assert!(!should_sync("workspace", Path::new("memory.db")));
        assert!(!should_sync(
            "workspace",
            Path::new("channels/group-1/memory.db")
        ));
        assert!(!should_sync(
            "workspace",
            Path::new("channels/group-1/.workspace_version")
        ));
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

    #[test]
    fn state_push_permanent_rejections_are_narrow() {
        assert!(is_permanent_state_file_rejection(
            reqwest::StatusCode::BAD_REQUEST
        ));
        assert!(is_permanent_state_file_rejection(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE
        ));
        assert!(!is_permanent_state_file_rejection(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!is_permanent_state_file_rejection(
            reqwest::StatusCode::FORBIDDEN
        ));
        assert!(!is_permanent_state_file_rejection(
            reqwest::StatusCode::CONFLICT
        ));
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
    async fn sync_once_skips_zero_byte_chat_transcripts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/state"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let chat = dir.path().join("c-1");
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(chat.join("transcript.json"), b"").unwrap();
        std::fs::write(chat.join("activity.jsonl"), b"{\"ok\":true}\n").unwrap();

        let cfg = StateSyncConfig {
            url: format!("{}/state", server.uri()),
            token: "st_test".into(),
        };
        let mut worker = StateSyncWorker::new(vec![SyncRoot {
            namespace: "chats",
            path: dir.path().to_path_buf(),
        }]);
        worker.sync_once(&cfg).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["namespace"], "chats");
        assert_eq!(body["path"], "c-1/activity.jsonl");
    }

    // Regression: a deterministic 4xx rejection (e.g. 413 payload too
    // large) used to be retried every 5s forever — full re-read +
    // re-base64 + re-POST at 0.2 Hz per file.  A 4xx must park the file
    // until its content actually changes.
    #[tokio::test]
    async fn rejected_4xx_file_is_not_retried_until_it_changes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/state"))
            .respond_with(ResponseTemplate::new(413))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MEMORY.md");
        std::fs::write(&file, "hello").unwrap();

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
        worker.sync_once(&cfg).await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a permanently-rejected file must not be re-POSTed while unchanged"
        );

        // Once the file changes (different stamp), it becomes eligible again.
        std::fs::write(&file, "hello, but smaller now?").unwrap();
        worker.sync_once(&cfg).await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "a changed file must be retried after a previous rejection"
        );
    }

    // Transient failures (5xx / network) must back off per file instead
    // of re-POSTing every 5s cycle.
    #[tokio::test]
    async fn transient_5xx_backs_off_between_cycles() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/state"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "hello").unwrap();

        let cfg = StateSyncConfig {
            url: format!("{}/state", server.uri()),
            token: "st_test".into(),
        };
        let mut worker = StateSyncWorker::new(vec![SyncRoot {
            namespace: "workspace",
            path: dir.path().to_path_buf(),
        }]);
        worker.sync_once(&cfg).await;
        // Immediately-following cycles are inside the backoff window.
        worker.sync_once(&cfg).await;
        worker.sync_once(&cfg).await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "back-to-back cycles must not re-POST a transiently-failing file"
        );
        // (record_error still fires on the failed push, but STATUS is a
        // process-wide global shared with concurrently-running tests, so
        // asserting on the snapshot here would be racy.)
    }

    // Regression: the 5s walk used to recurse into every subdirectory —
    // including hidden ones like `.git`, whose contents can never sync
    // (should_sync rejects any hidden path component).  A workspace .git
    // tree alone is tens of thousands of entries scanned at 0.2 Hz.
    #[test]
    fn walk_never_descends_into_hidden_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "hello").unwrap();
        let hidden = dir.path().join(".git").join("objects").join("aa");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("blob"), "junk").unwrap();
        let visible = dir.path().join("kb");
        std::fs::create_dir_all(&visible).unwrap();
        std::fs::write(visible.join("facts.md"), "fact").unwrap();
        let node_modules = dir.path().join("node_modules").join("pkg");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("README.md"), "junk").unwrap();
        let target = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("README.md"), "junk").unwrap();

        walk_probe::start();
        let files = collect_files(&[SyncRoot {
            namespace: "workspace",
            path: dir.path().to_path_buf(),
        }]);
        let visited = walk_probe::take();

        // Output unchanged: only syncable files collected.
        let keys: BTreeSet<String> = files.into_iter().map(|f| f.key).collect();
        assert!(keys.contains("workspace:MEMORY.md"));
        assert!(keys.contains("workspace:kb/facts.md"));
        assert_eq!(keys.len(), 2);

        // But the walk itself must have pruned hidden and visible
        // non-syncable subtrees.
        // (Compare paths relative to the walk root — the tempdir itself
        // has a hidden `.tmpXXXX` name.)
        let pruned_visits: Vec<_> = visited
            .iter()
            .filter_map(|p| p.strip_prefix(dir.path()).ok())
            .filter(|rel| {
                rel.components().any(|c| {
                    matches!(c, Component::Normal(s)
                        if s.to_str().is_some_and(|s| s.starts_with('.')))
                        || matches!(c, Component::Normal(s)
                            if matches!(s.to_str(), Some("node_modules" | "target")))
                })
            })
            .collect();
        assert!(
            pruned_visits.is_empty(),
            "the walk must not descend into pruned directories: {pruned_visits:?}"
        );
        assert!(
            visited.iter().any(|p| p.ends_with("kb")),
            "syncable subdirectories must still be walked: {visited:?}"
        );
    }

    #[test]
    fn transient_backoff_schedule_grows_and_caps() {
        // First failure waits at least one full sync interval, doubling
        // after that, capped so a file can never be parked forever.
        let mut prev = Duration::ZERO;
        for failures in 1..=16u32 {
            let d = transient_backoff(failures);
            assert!(
                d >= SYNC_INTERVAL,
                "backoff must be at least one sync interval, got {d:?}"
            );
            assert!(d >= prev, "backoff must be monotonic");
            assert!(
                d <= MAX_TRANSIENT_BACKOFF,
                "backoff must cap at {MAX_TRANSIENT_BACKOFF:?}, got {d:?}"
            );
            prev = d;
        }
        assert_eq!(transient_backoff(1), SYNC_INTERVAL * 2);
        assert_eq!(transient_backoff(2), SYNC_INTERVAL * 4);
        assert_eq!(transient_backoff(30), MAX_TRANSIENT_BACKOFF);
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
