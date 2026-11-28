// ===========================================================================
// Swarm brokered artifact export protocol (Box routes 1–4).
//
// Bodies at or below `INLINE_THRESHOLD_BYTES` (256 KiB) keep the legacy
// base64 JSON ingest POST (`{ingest_url}` with `it_` bearer).
//
// Bodies above `INLINE_THRESHOLD_BYTES` use Swarm's brokered export flow:
//   1. POST {origin}/v1/internal/artifacts/uploads   (pt_ + workload identity)
//   2. PUT  <presigned urls[i]>                      (bytes + Content-Length only)
//   3. POST {origin}/v1/internal/artifacts/uploads/{id}/complete
//   4. POST {origin}/v1/internal/artifacts/uploads/{id}/urls  (re-sign on expiry)
//
// Files on disk stream in 64 KiB chunks directly from `tokio::fs::File` into
// `reqwest::Body::wrap_stream`, so multi-gigabyte uploads stay O(1) in RAM.
// Neither the `pt_` bearer nor any presigned URL is ever logged or formatted
// into error messages (`PresignedUrl` redacts `Debug`, and every `reqwest::Error`
// is stripped with `without_url()`).
// ===========================================================================

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::auth::workload_identity;

use super::state::{ExportConfig, IngestConfig};
use super::wire::SseEvent;

/// Bodies strictly larger than this take the brokered export protocol;
/// bodies at or below it keep the legacy base64 ingest POST unchanged.
pub(crate) const INLINE_THRESHOLD_BYTES: u64 = 256 * 1024;

/// Maximum body size the legacy base64 ingest endpoint (and in-memory
/// `FileStore` buffer) accepts.
pub(crate) const MAX_LEGACY_INGEST_BYTES: u64 = 25 * 1024 * 1024;

/// Sanity cap for disk-streamed exports (1 TiB — matches Swarm's hard ceiling
/// on admin per-object limit overrides).  Dyson streams in 64 KiB chunks so
/// memory usage is independent of file size; Swarm enforces the owner's
/// effective per-object limit (default 5 GiB) and quota.
pub(crate) const MAX_EXPORT_FILE_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

/// Chunk size when streaming a disk file into an HTTP PUT body.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Re-sign presigned URLs proactively when fewer than this many seconds
/// remain before `expires_at`.
const RESIGN_LEEWAY_SECS: i64 = 30;

/// Maximum number of URL re-signs permitted in a single export.
const MAX_RESIGNS: u32 = 8;

/// Maximum byte length of Swarm JSON error response bodies we read.
const MAX_ERROR_BODY_BYTES: usize = 4096;

/// Redacting wrapper around a presigned object-store URL so it can never leak
/// via `Debug` output or accidental formatting.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
struct PresignedUrl(String);

impl PresignedUrl {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PresignedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED_URL]")
    }
}

/// Source of bytes for an artefact push.
#[derive(Clone)]
pub(crate) enum ExportSource {
    /// Already in memory (`Bytes` slices are zero-copy `Arc` clones).
    Bytes(Bytes),
    /// On disk — streamed in 64 KiB chunks per part without reading the whole
    /// file into memory.
    DiskFile { path: PathBuf, len: u64 },
}

impl ExportSource {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::DiskFile { len, .. } => *len,
        }
    }

    async fn read_all_for_legacy_ingest(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::Bytes(b) => Ok(b.to_vec()),
            Self::DiskFile { path, .. } => tokio::fs::read(path)
                .await
                .map_err(|e| format!("read file for legacy ingest failed: {e}")),
        }
    }

    fn part_body(&self, offset: u64, part_len: u64) -> Result<reqwest::Body, String> {
        match self {
            Self::Bytes(b) => {
                let start = usize::try_from(offset)
                    .map_err(|_| "part offset overflows usize".to_string())?;
                let len = usize::try_from(part_len)
                    .map_err(|_| "part length overflows usize".to_string())?;
                let end = start
                    .checked_add(len)
                    .ok_or_else(|| "part range overflow".to_string())?;
                if end > b.len() {
                    return Err("part range exceeds buffer length".to_string());
                }
                Ok(reqwest::Body::from(b.slice(start..end)))
            }
            Self::DiskFile { path, .. } => Ok(file_slice_body(path.clone(), offset, part_len)),
        }
    }
}

/// Stream `[offset, offset + len)` from `path` in `STREAM_CHUNK_BYTES` chunks.
fn file_slice_body(path: PathBuf, offset: u64, len: u64) -> reqwest::Body {
    let stream = async_stream::stream! {
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                yield Err::<Bytes, std::io::Error>(e);
                return;
            }
        };
        if let Err(e) = tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(offset)).await {
            yield Err::<Bytes, std::io::Error>(e);
            return;
        }
        let mut remaining = len;
        let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
        while remaining > 0 {
            let to_read = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
            match tokio::io::AsyncReadExt::read(&mut file, &mut buf[..to_read]).await {
                Ok(0) => {
                    yield Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file shrank during export upload",
                    ));
                    return;
                }
                Ok(n) => {
                    remaining -= n as u64;
                    yield Ok(Bytes::copy_from_slice(&buf[..n]));
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
    };
    reqwest::Body::wrap_stream(stream)
}

/// Metadata carried into both the export protocol and legacy ingest fallback.
#[derive(Clone)]
pub(crate) struct ExportMetadata {
    pub(crate) chat_id: String,
    pub(crate) artefact_id: String,
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) mime: String,
    pub(crate) filename: String,
    pub(crate) metadata: Option<serde_json::Value>,
    pub(crate) created_at: u64,
}

/// Retry and timeout budget for the export protocol.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExportRetryPolicy {
    pub(crate) max_attempts: u32,
    pub(crate) initial_backoff: Duration,
    pub(crate) max_backoff: Duration,
    pub(crate) total_timeout: Duration,
    pub(crate) control_timeout: Duration,
    pub(crate) put_timeout: Duration,
}

impl Default for ExportRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(10),
            total_timeout: Duration::from_secs(15 * 60),
            control_timeout: Duration::from_secs(30),
            put_timeout: Duration::from_secs(300),
        }
    }
}

impl ExportRetryPolicy {
    fn backoff_for(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
        self.initial_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExportOutcome {
    Exported,
    AlreadyExists,
    FallbackToLegacy {
        code: &'static str,
    },
    Rejected {
        code: String,
        status: u16,
        message: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Serialize)]
struct ReserveRequest<'a> {
    filename: &'a str,
    mime: &'a str,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_id: Option<&'a str>,
    kind: &'a str,
}

#[derive(Deserialize)]
struct ReserveResponse {
    artifact_id: String,
    #[allow(dead_code)]
    chat_id: String,
    multipart: bool,
    part_size: u64,
    urls: Vec<PresignedUrl>,
    expires_at: i64,
    #[allow(dead_code)]
    url: String,
}

#[derive(Deserialize)]
struct ResignResponse {
    #[allow(dead_code)]
    multipart: bool,
    part_size: u64,
    urls: Vec<PresignedUrl>,
    #[serde(default)]
    uploaded_parts: Vec<u32>,
    expires_at: i64,
}

#[derive(Deserialize, Default)]
struct SwarmErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    max_object_bytes: Option<u64>,
    #[serde(default)]
    quota_bytes: Option<u64>,
    #[serde(default)]
    used_bytes: Option<u64>,
}

/// Top-level entry point called by `SseOutput::send_artefact`'s background task.
pub(crate) async fn push_artefact(
    ingest: Option<IngestConfig>,
    export: Option<ExportConfig>,
    source: ExportSource,
    meta: ExportMetadata,
    policy: ExportRetryPolicy,
    notice_tx: Option<broadcast::Sender<SseEvent>>,
) {
    let body_len = source.len();
    if body_len <= INLINE_THRESHOLD_BYTES {
        if let Some(cfg) = ingest {
            push_legacy_ingest(&cfg, &source, &meta).await;
        }
        return;
    }

    let Some(export_cfg) = export else {
        if let Some(cfg) = ingest {
            if body_len <= MAX_LEGACY_INGEST_BYTES {
                push_legacy_ingest(&cfg, &source, &meta).await;
            } else {
                report_over_cap_unavailable(&meta, body_len, "export_not_configured", &notice_tx);
            }
        }
        return;
    };

    match export_to_swarm(&export_cfg, &source, &meta, &policy).await {
        ExportOutcome::Exported => {
            tracing::debug!(
                artefact = %meta.artefact_id,
                chat = %meta.chat_id,
                bytes = body_len,
                "export: artifact exported to swarm",
            );
        }
        ExportOutcome::AlreadyExists => {
            tracing::debug!(
                artefact = %meta.artefact_id,
                chat = %meta.chat_id,
                "export: artifact already exists on swarm; treating as success",
            );
        }
        ExportOutcome::FallbackToLegacy { code } => {
            if body_len <= MAX_LEGACY_INGEST_BYTES {
                if let Some(cfg) = ingest {
                    tracing::info!(
                        artefact = %meta.artefact_id,
                        chat = %meta.chat_id,
                        code,
                        bytes = body_len,
                        "export: unavailable on swarm; falling back to legacy inline ingest",
                    );
                    push_legacy_ingest(&cfg, &source, &meta).await;
                } else {
                    tracing::warn!(
                        artefact = %meta.artefact_id,
                        chat = %meta.chat_id,
                        code,
                        "export: unavailable on swarm and no legacy ingest configured",
                    );
                    emit_live_notice(
                        &notice_tx,
                        format!(
                            "\n[artefact {}: export to Swarm failed ({code}): artifact export is unavailable]\n",
                            meta.title,
                        ),
                    );
                }
            } else {
                report_over_cap_unavailable(&meta, body_len, code, &notice_tx);
            }
        }
        ExportOutcome::Rejected {
            code,
            status,
            message,
        } => {
            tracing::warn!(
                artefact = %meta.artefact_id,
                chat = %meta.chat_id,
                code = %code,
                status,
                detail = %message,
                "export: swarm rejected artefact export",
            );
            emit_live_notice(
                &notice_tx,
                format!(
                    "\n[artefact {}: export to Swarm rejected ({code}): {message}]\n",
                    meta.title,
                ),
            );
        }
        ExportOutcome::Failed { message } => {
            tracing::warn!(
                artefact = %meta.artefact_id,
                chat = %meta.chat_id,
                error = %message,
                "export: swarm artefact export failed",
            );
            emit_live_notice(
                &notice_tx,
                format!(
                    "\n[artefact {}: export to Swarm failed: {message}]\n",
                    meta.title,
                ),
            );
        }
    }
}

fn report_over_cap_unavailable(
    meta: &ExportMetadata,
    body_len: u64,
    code: &'static str,
    notice_tx: &Option<broadcast::Sender<SseEvent>>,
) {
    let mb = body_len / (1024 * 1024);
    tracing::warn!(
        artefact = %meta.artefact_id,
        chat = %meta.chat_id,
        code,
        bytes = body_len,
        "export: unavailable and body exceeds 25 MiB legacy ingest cap",
    );
    emit_live_notice(
        notice_tx,
        format!(
            "\n[file: {} too large ({mb} MB) and artifact export unavailable ({code}) — not delivered to Swarm]\n",
            meta.filename,
        ),
    );
}

/// Send a live-only SSE notice without touching the replay ring so a late
/// background failure can inform an active viewer without polluting the next
/// turn's replay buffer.
fn emit_live_notice(tx: &Option<broadcast::Sender<SseEvent>>, delta: String) {
    if let Some(tx) = tx {
        let _ = tx.send(SseEvent::Text { delta });
    }
}

/// Legacy base64 JSON POST to `{ingest_url}` with `it_` bearer.
async fn push_legacy_ingest(cfg: &IngestConfig, source: &ExportSource, meta: &ExportMetadata) {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;

    let push_bytes = match source.read_all_for_legacy_ingest().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                artefact = %meta.artefact_id,
                error = %e,
                "ingest: failed to read artefact bytes",
            );
            return;
        }
    };
    let payload = serde_json::json!({
        "chat_id": meta.chat_id,
        "artefact_id": meta.artefact_id,
        "kind": meta.kind,
        "title": meta.title,
        "mime": meta.mime,
        "metadata": meta.metadata,
        "created_at": meta.created_at as i64,
        "body_b64": B64.encode(&push_bytes),
    });
    let r = crate::http::client()
        .post(&cfg.url)
        .bearer_auth(&cfg.token)
        .timeout(Duration::from_secs(10))
        .json(&payload)
        .send()
        .await;
    match r {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(
                artefact = %meta.artefact_id,
                chat = %meta.chat_id,
                "ingest: pushed",
            );
        }
        Ok(resp) => {
            tracing::warn!(
                artefact = %meta.artefact_id,
                status = %resp.status(),
                "ingest: swarm rejected artefact push",
            );
        }
        Err(e) => {
            let detail = sanitize_reqwest_error(e);
            tracing::warn!(
                artefact = %meta.artefact_id,
                error = %detail,
                "ingest: swarm push failed (network/timeout)",
            );
        }
    }
}

/// Execute the brokered export protocol (reserve → PUT part(s) → complete)
/// against Swarm and the presigned object-store URLs.
pub(crate) async fn export_to_swarm(
    cfg: &ExportConfig,
    source: &ExportSource,
    meta: &ExportMetadata,
    policy: &ExportRetryPolicy,
) -> ExportOutcome {
    let deadline = Instant::now() + policy.total_timeout;
    let total_bytes = match resolve_actual_len(source).await {
        Ok(len) if len > 0 => len,
        Ok(_) => {
            return ExportOutcome::Failed {
                message: "cannot export empty artefact".to_string(),
            };
        }
        Err(message) => return ExportOutcome::Failed { message },
    };

    let reservation = match reserve_upload(cfg, meta, total_bytes, policy, deadline).await {
        ReserveResult::Reserved(r) => r,
        ReserveResult::Outcome(outcome) => return outcome,
    };

    let reserved_id = reservation.artifact_id.clone();
    if let Err(outcome) =
        upload_all_parts(cfg, source, total_bytes, reservation, policy, deadline).await
    {
        return outcome;
    }

    complete_upload(cfg, &reserved_id, policy, deadline).await
}

async fn resolve_actual_len(source: &ExportSource) -> Result<u64, String> {
    match source {
        ExportSource::Bytes(b) => Ok(b.len() as u64),
        ExportSource::DiskFile { path, .. } => tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .map_err(|e| format!("stat file before export failed: {e}")),
    }
}

enum ReserveResult {
    Reserved(ReserveResponse),
    Outcome(ExportOutcome),
}

async fn reserve_upload(
    cfg: &ExportConfig,
    meta: &ExportMetadata,
    total_bytes: u64,
    policy: &ExportRetryPolicy,
    deadline: Instant,
) -> ReserveResult {
    let filename = sanitize_contract_filename(&meta.filename, &meta.artefact_id);
    let mime = sanitize_contract_mime(&meta.mime);
    let title = sanitize_contract_title(&meta.title);
    let kind = sanitize_contract_kind(&meta.kind);
    let chat_id = valid_contract_id(&meta.chat_id).then_some(meta.chat_id.as_str());
    let artifact_id = valid_contract_id(&meta.artefact_id).then_some(meta.artefact_id.as_str());

    let req_body = ReserveRequest {
        filename: &filename,
        mime: &mime,
        bytes: total_bytes,
        title: title.as_deref(),
        chat_id,
        artifact_id,
        kind: &kind,
    };

    let url = format!(
        "{}/v1/internal/artifacts/uploads",
        cfg.origin.trim_end_matches('/')
    );
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err = String::new();

    for attempt in 0..max_attempts {
        let Some(req_timeout) = remaining_timeout(deadline, policy.control_timeout) else {
            return ReserveResult::Outcome(ExportOutcome::Failed {
                message: "export timed out before reserve".to_string(),
            });
        };

        let req = swarm_post(&url, &cfg.token)
            .timeout(req_timeout)
            .json(&req_body);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return match resp.json::<ReserveResponse>().await {
                        Ok(parsed) => ReserveResult::Reserved(parsed),
                        Err(e) => ReserveResult::Outcome(ExportOutcome::Failed {
                            message: format!(
                                "invalid reserve response JSON: {}",
                                sanitize_reqwest_error(e)
                            ),
                        }),
                    };
                }
                let err_body = read_swarm_error(resp).await;
                if status == reqwest::StatusCode::CONFLICT && err_body.code == "artifact_exists" {
                    return ReserveResult::Outcome(ExportOutcome::AlreadyExists);
                }
                if status == reqwest::StatusCode::NOT_IMPLEMENTED {
                    return ReserveResult::Outcome(ExportOutcome::FallbackToLegacy {
                        code: "artifacts_disabled",
                    });
                }
                if status == reqwest::StatusCode::NOT_FOUND {
                    return ReserveResult::Outcome(ExportOutcome::FallbackToLegacy {
                        code: "not_found",
                    });
                }
                if is_transient_status(status) {
                    last_err = format_swarm_error(status, &err_body);
                } else {
                    let code = non_empty_code(status, &err_body);
                    let message = format_swarm_error(status, &err_body);
                    return ReserveResult::Outcome(ExportOutcome::Rejected {
                        code,
                        status: status.as_u16(),
                        message,
                    });
                }
            }
            Err(e) => {
                last_err = sanitize_reqwest_error(e);
            }
        }

        if !sleep_backoff(policy, attempt, max_attempts, deadline).await {
            break;
        }
    }

    ReserveResult::Outcome(ExportOutcome::Failed {
        message: format!("reserve upload failed: {last_err}"),
    })
}

async fn upload_all_parts(
    cfg: &ExportConfig,
    source: &ExportSource,
    total_bytes: u64,
    reservation: ReserveResponse,
    policy: &ExportRetryPolicy,
    deadline: Instant,
) -> Result<(), ExportOutcome> {
    let part_size = if reservation.multipart {
        if reservation.part_size == 0 {
            return Err(ExportOutcome::Failed {
                message: "swarm returned multipart upload with part_size=0".to_string(),
            });
        }
        reservation.part_size
    } else {
        total_bytes
    };

    let expected_parts = if reservation.multipart {
        usize::try_from(total_bytes.div_ceil(part_size)).map_err(|_| ExportOutcome::Failed {
            message: "multipart part count overflow".to_string(),
        })?
    } else {
        1
    };

    let mut urls = reservation.urls;
    let mut expires_at = reservation.expires_at;
    let mut uploaded_parts: HashSet<u32> = HashSet::new();
    let mut resign_count: u32 = 0;

    if urls.len() < expected_parts {
        return Err(ExportOutcome::Failed {
            message: format!(
                "swarm returned {} upload URLs for {expected_parts} parts",
                urls.len()
            ),
        });
    }

    for part_idx in 0..expected_parts {
        let part_num = u32::try_from(part_idx + 1).map_err(|_| ExportOutcome::Failed {
            message: "part number overflow".to_string(),
        })?;
        if uploaded_parts.contains(&part_num) {
            continue;
        }

        let offset = (part_idx as u64) * part_size;
        let part_len = (total_bytes - offset).min(part_size);
        let mut resigned_this_part = false;

        loop {
            if expires_at > 0
                && now_unix_secs() >= expires_at - RESIGN_LEEWAY_SECS
                && !resigned_this_part
            {
                if resign_count >= MAX_RESIGNS {
                    return Err(ExportOutcome::Failed {
                        message: "exceeded maximum presigned URL re-signs".to_string(),
                    });
                }
                let refreshed =
                    resign_urls(cfg, &reservation.artifact_id, policy, deadline).await?;
                resign_count += 1;
                resigned_this_part = true;
                urls = refreshed.urls;
                expires_at = refreshed.expires_at;
                uploaded_parts.extend(refreshed.uploaded_parts);
                if uploaded_parts.contains(&part_num) {
                    break;
                }
                if urls.len() <= part_idx {
                    return Err(ExportOutcome::Failed {
                        message: format!(
                            "re-signed upload returned {} URLs for part {part_num}",
                            urls.len()
                        ),
                    });
                }
                if reservation.multipart && refreshed.part_size != part_size {
                    return Err(ExportOutcome::Failed {
                        message: "swarm changed part_size during re-sign".to_string(),
                    });
                }
            }

            match put_part_with_retries(source, offset, part_len, &urls[part_idx], policy, deadline)
                .await
            {
                PutOutcome::Uploaded => {
                    uploaded_parts.insert(part_num);
                    break;
                }
                PutOutcome::Forbidden if !resigned_this_part && resign_count < MAX_RESIGNS => {
                    let refreshed =
                        resign_urls(cfg, &reservation.artifact_id, policy, deadline).await?;
                    resign_count += 1;
                    resigned_this_part = true;
                    urls = refreshed.urls;
                    expires_at = refreshed.expires_at;
                    uploaded_parts.extend(refreshed.uploaded_parts);
                    if uploaded_parts.contains(&part_num) {
                        break;
                    }
                    if urls.len() <= part_idx {
                        return Err(ExportOutcome::Failed {
                            message: format!(
                                "re-signed upload returned {} URLs for part {part_num}",
                                urls.len()
                            ),
                        });
                    }
                }
                PutOutcome::Forbidden => {
                    return Err(ExportOutcome::Failed {
                        message: format!("object store rejected part {part_num} with HTTP 403"),
                    });
                }
                PutOutcome::Failed(msg) => {
                    return Err(ExportOutcome::Failed {
                        message: format!("upload part {part_num}/{expected_parts} failed: {msg}"),
                    });
                }
            }
        }
    }

    Ok(())
}

enum PutOutcome {
    Uploaded,
    Forbidden,
    Failed(String),
}

async fn put_part_with_retries(
    source: &ExportSource,
    offset: u64,
    part_len: u64,
    presigned_url: &PresignedUrl,
    policy: &ExportRetryPolicy,
    deadline: Instant,
) -> PutOutcome {
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err = String::new();

    for attempt in 0..max_attempts {
        let Some(req_timeout) = remaining_timeout(deadline, policy.put_timeout) else {
            return PutOutcome::Failed("total export timeout exceeded".to_string());
        };
        let body = match source.part_body(offset, part_len) {
            Ok(b) => b,
            Err(e) => return PutOutcome::Failed(e),
        };

        // Wire contract Box route 2: PUT <urls[i]> with the bytes and
        // Content-Length, no other headers (no Authorization, no workload
        // identity, no Content-Type). Use the shared no-redirect client so
        // proxy/CA settings apply and redirects can never leak the signed URL.
        let result = crate::http::no_redirect_client()
            .put(presigned_url.as_str())
            .header(reqwest::header::CONTENT_LENGTH, part_len)
            .timeout(req_timeout)
            .body(body)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return PutOutcome::Uploaded;
                }
                if status == reqwest::StatusCode::FORBIDDEN {
                    return PutOutcome::Forbidden;
                }
                if is_transient_status(status) {
                    last_err = format!("object store HTTP {}", status.as_u16());
                } else {
                    return PutOutcome::Failed(format!("object store HTTP {}", status.as_u16()));
                }
            }
            Err(e) => {
                last_err = sanitize_reqwest_error(e);
            }
        }

        if !sleep_backoff(policy, attempt, max_attempts, deadline).await {
            break;
        }
    }

    PutOutcome::Failed(last_err)
}

async fn resign_urls(
    cfg: &ExportConfig,
    artifact_id: &str,
    policy: &ExportRetryPolicy,
    deadline: Instant,
) -> Result<ResignResponse, ExportOutcome> {
    let url = format!(
        "{}/v1/internal/artifacts/uploads/{artifact_id}/urls",
        cfg.origin.trim_end_matches('/')
    );
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err = String::new();

    for attempt in 0..max_attempts {
        let Some(req_timeout) = remaining_timeout(deadline, policy.control_timeout) else {
            return Err(ExportOutcome::Failed {
                message: "export timed out during URL re-sign".to_string(),
            });
        };

        let result = swarm_post(&url, &cfg.token)
            .timeout(req_timeout)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp.json::<ResignResponse>().await.map_err(|e| {
                        ExportOutcome::Failed {
                            message: format!(
                                "invalid re-sign response JSON: {}",
                                sanitize_reqwest_error(e)
                            ),
                        }
                    });
                }
                let err_body = read_swarm_error(resp).await;
                if is_transient_status(status) {
                    last_err = format_swarm_error(status, &err_body);
                } else {
                    return Err(ExportOutcome::Rejected {
                        code: non_empty_code(status, &err_body),
                        status: status.as_u16(),
                        message: format_swarm_error(status, &err_body),
                    });
                }
            }
            Err(e) => {
                last_err = sanitize_reqwest_error(e);
            }
        }

        if !sleep_backoff(policy, attempt, max_attempts, deadline).await {
            break;
        }
    }

    Err(ExportOutcome::Failed {
        message: format!("re-sign upload URLs failed: {last_err}"),
    })
}

async fn complete_upload(
    cfg: &ExportConfig,
    artifact_id: &str,
    policy: &ExportRetryPolicy,
    deadline: Instant,
) -> ExportOutcome {
    let url = format!(
        "{}/v1/internal/artifacts/uploads/{artifact_id}/complete",
        cfg.origin.trim_end_matches('/')
    );
    let max_attempts = policy.max_attempts.max(1);
    let mut last_err = String::new();

    for attempt in 0..max_attempts {
        let Some(req_timeout) = remaining_timeout(deadline, policy.control_timeout) else {
            return ExportOutcome::Failed {
                message: "export timed out before complete".to_string(),
            };
        };

        let result = swarm_post(&url, &cfg.token)
            .timeout(req_timeout)
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return ExportOutcome::Exported;
                }
                let err_body = read_swarm_error(resp).await;
                if is_transient_status(status) {
                    last_err = format_swarm_error(status, &err_body);
                } else {
                    return ExportOutcome::Rejected {
                        code: non_empty_code(status, &err_body),
                        status: status.as_u16(),
                        message: format_swarm_error(status, &err_body),
                    };
                }
            }
            Err(e) => {
                last_err = sanitize_reqwest_error(e);
            }
        }

        if !sleep_backoff(policy, attempt, max_attempts, deadline).await {
            break;
        }
    }

    ExportOutcome::Failed {
        message: format!("complete upload failed: {last_err}"),
    }
}

/// Build an authenticated POST to a Swarm internal route using the shared
/// no-redirect HTTP client (so `X-Dyson-Workload-Identity` and `pt_` can never
/// be forwarded across origins on a redirect).
fn swarm_post(url: &str, proxy_token: &str) -> reqwest::RequestBuilder {
    let req = crate::http::no_redirect_client()
        .post(url)
        .bearer_auth(proxy_token);
    workload_identity::attach(req, proxy_token)
}

fn is_transient_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

fn remaining_timeout(deadline: Instant, per_request: Duration) -> Option<Duration> {
    let rem = deadline.checked_duration_since(Instant::now())?;
    if rem.is_zero() {
        return None;
    }
    Some(rem.min(per_request))
}

async fn sleep_backoff(
    policy: &ExportRetryPolicy,
    attempt: u32,
    max_attempts: u32,
    deadline: Instant,
) -> bool {
    if attempt + 1 >= max_attempts {
        return false;
    }
    let delay = policy.backoff_for(attempt);
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return false;
    };
    if remaining <= delay {
        return false;
    }
    tokio::time::sleep(delay).await;
    true
}

async fn read_swarm_error(resp: reqwest::Response) -> SwarmErrorBody {
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => return SwarmErrorBody::default(),
    };
    let slice = &bytes[..bytes.len().min(MAX_ERROR_BODY_BYTES)];
    serde_json::from_slice::<SwarmErrorBody>(slice).unwrap_or_default()
}

fn non_empty_code(status: reqwest::StatusCode, body: &SwarmErrorBody) -> String {
    if !body.code.is_empty() {
        body.code.clone()
    } else {
        format!("http_{}", status.as_u16())
    }
}

fn format_swarm_error(status: reqwest::StatusCode, body: &SwarmErrorBody) -> String {
    let mut msg = if !body.error.is_empty() {
        truncate_utf8(&body.error, 512).to_string()
    } else if !body.code.is_empty() {
        body.code.clone()
    } else {
        format!("HTTP {}", status.as_u16())
    };
    if let Some(max) = body.max_object_bytes {
        msg.push_str(&format!(" (max_object_bytes={max})"));
    }
    if let (Some(used), Some(quota)) = (body.used_bytes, body.quota_bytes) {
        msg.push_str(&format!(" (used_bytes={used}, quota_bytes={quota})"));
    }
    msg
}

/// Format a `reqwest::Error` with its URL stripped so presigned URLs (and
/// query credentials) can never appear in logs or user-facing error messages.
fn sanitize_reqwest_error(err: reqwest::Error) -> String {
    err.without_url().to_string()
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn valid_contract_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Wire contract: `filename` 1-255 bytes, no `/`, `\`, NUL or control chars.
fn sanitize_contract_filename(raw: &str, fallback_id: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '/' | '\\') && !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        let fallback = if valid_contract_id(fallback_id) {
            format!("{fallback_id}.bin")
        } else {
            "artifact.bin".to_string()
        };
        return truncate_utf8(&fallback, 255).to_string();
    }
    truncate_utf8(trimmed, 255).to_string()
}

/// Wire contract: `mime` up to 127 bytes, default `application/octet-stream`.
fn sanitize_contract_mime(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "application/octet-stream".to_string()
    } else {
        truncate_utf8(trimmed, 127).to_string()
    }
}

/// Wire contract: `title` up to 200 bytes.
fn sanitize_contract_title(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate_utf8(trimmed, 200).to_string())
    }
}

/// Wire contract: `kind` up to 64 bytes, default `file`.
fn sanitize_contract_kind(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        truncate_utf8(trimmed, 64).to_string()
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::prelude::*;

    fn fast_policy() -> ExportRetryPolicy {
        ExportRetryPolicy {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            total_timeout: Duration::from_secs(5),
            control_timeout: Duration::from_secs(2),
            put_timeout: Duration::from_secs(2),
        }
    }

    fn test_meta(id: &str, filename: &str) -> ExportMetadata {
        ExportMetadata {
            chat_id: "c-0001".to_string(),
            artefact_id: id.to_string(),
            kind: "other".to_string(),
            title: filename.to_string(),
            mime: "video/mp4".to_string(),
            filename: filename.to_string(),
            metadata: Some(serde_json::json!({ "file_name": filename })),
            created_at: 1_700_000_000,
        }
    }

    #[derive(Clone, Debug)]
    struct RecordedReq {
        method: String,
        path: String,
        query: Option<String>,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    enum ReserveBehavior {
        SinglePut,
        Multipart {
            part_size: u64,
        },
        JsonError {
            status: u16,
            body: serde_json::Value,
        },
        BareStatus(u16),
    }

    #[derive(Clone)]
    enum PutBehavior {
        Ok,
        FailFirst { failures: usize, status: u16 },
        AlwaysStatus(u16),
        ExpirePartOnce { part_num: u32 },
        Hang,
    }

    struct MockState {
        reserve_behavior: ReserveBehavior,
        put_behavior: PutBehavior,
        object_store_origin: String,
        swarm_origin: String,
        swarm_requests: Vec<RecordedReq>,
        store_requests: Vec<RecordedReq>,
        put_counts: HashMap<u32, usize>,
        expired_once: bool,
        uploaded_parts: Vec<u32>,
    }

    struct MockHarness {
        swarm_origin: String,
        state: Arc<Mutex<MockState>>,
    }

    impl MockHarness {
        async fn start(reserve_behavior: ReserveBehavior, put_behavior: PutBehavior) -> Self {
            let store_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let store_addr = store_listener.local_addr().unwrap();
            let swarm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let swarm_addr = swarm_listener.local_addr().unwrap();

            let state = Arc::new(Mutex::new(MockState {
                reserve_behavior,
                put_behavior,
                object_store_origin: format!("http://{store_addr}"),
                swarm_origin: format!("http://{swarm_addr}"),
                swarm_requests: Vec::new(),
                store_requests: Vec::new(),
                put_counts: HashMap::new(),
                expired_once: false,
                uploaded_parts: Vec::new(),
            }));

            let store_state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = store_listener.accept().await else {
                        break;
                    };
                    let st = Arc::clone(&store_state);
                    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                        let st = Arc::clone(&st);
                        async move { handle_store_req(req, st).await }
                    });
                    tokio::spawn(async move {
                        let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
            });

            let swarm_state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = swarm_listener.accept().await else {
                        break;
                    };
                    let st = Arc::clone(&swarm_state);
                    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                        let st = Arc::clone(&st);
                        async move { handle_swarm_req(req, st).await }
                    });
                    tokio::spawn(async move {
                        let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
            });

            Self {
                swarm_origin: format!("http://{swarm_addr}"),
                state,
            }
        }

        fn ingest_cfg(&self) -> IngestConfig {
            IngestConfig {
                url: format!("{}/v1/internal/ingest/artefact", self.swarm_origin),
                token: "it_11111111111111111111111111111111".to_string(),
            }
        }

        fn export_cfg(&self) -> ExportConfig {
            ExportConfig {
                origin: self.swarm_origin.clone(),
                token: "pt_secret_token_999999999999999999999999".to_string(),
            }
        }
    }

    async fn record_req(req: hyper::Request<Incoming>) -> RecordedReq {
        let method = req.method().as_str().to_string();
        let path = req.uri().path().to_string();
        let query = req.uri().query().map(str::to_string);
        let headers = req
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_lowercase(), v.to_str().ok()?.to_string())))
            .collect();
        let body = req
            .into_body()
            .collect()
            .await
            .map(|c| c.to_bytes().to_vec())
            .unwrap_or_default();
        RecordedReq {
            method,
            path,
            query,
            headers,
            body,
        }
    }

    fn resp_json(status: u16, value: &serde_json::Value) -> hyper::Response<Full<Bytes>> {
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(value.to_string())))
            .unwrap()
    }

    fn resp_empty(status: u16) -> hyper::Response<Full<Bytes>> {
        hyper::Response::builder()
            .status(status)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    async fn handle_store_req(
        req: hyper::Request<Incoming>,
        state: Arc<Mutex<MockState>>,
    ) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
        let behavior = state.lock().unwrap().put_behavior.clone();
        if matches!(behavior, PutBehavior::Hang) {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return Ok(resp_empty(504));
        }

        let rec = record_req(req).await;
        let part_num: u32 = rec
            .path
            .rsplit('/')
            .next()
            .and_then(|s| s.strip_prefix("part-"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        let mut st = state.lock().unwrap();
        let count = st.put_counts.entry(part_num).or_insert(0);
        *count += 1;
        let attempt = *count;
        let behavior = st.put_behavior.clone();
        st.store_requests.push(rec);

        match behavior {
            PutBehavior::Ok => {
                if !st.uploaded_parts.contains(&part_num) {
                    st.uploaded_parts.push(part_num);
                }
                Ok(resp_empty(200))
            }
            PutBehavior::FailFirst { failures, status } => {
                if attempt <= failures {
                    Ok(resp_empty(status))
                } else {
                    if !st.uploaded_parts.contains(&part_num) {
                        st.uploaded_parts.push(part_num);
                    }
                    Ok(resp_empty(200))
                }
            }
            PutBehavior::AlwaysStatus(status) => Ok(resp_empty(status)),
            PutBehavior::ExpirePartOnce { part_num: target } => {
                if part_num == target && !st.expired_once {
                    st.expired_once = true;
                    Ok(resp_empty(403))
                } else {
                    if !st.uploaded_parts.contains(&part_num) {
                        st.uploaded_parts.push(part_num);
                    }
                    Ok(resp_empty(200))
                }
            }
            PutBehavior::Hang => Ok(resp_empty(504)),
        }
    }

    async fn handle_swarm_req(
        req: hyper::Request<Incoming>,
        state: Arc<Mutex<MockState>>,
    ) -> Result<hyper::Response<Full<Bytes>>, std::convert::Infallible> {
        let rec = record_req(req).await;
        let path = rec.path.clone();
        let body_bytes = rec.body.clone();

        let mut st = state.lock().unwrap();
        st.swarm_requests.push(rec);

        if path == "/v1/internal/ingest/artefact" {
            return Ok(resp_empty(204));
        }

        if path == "/v1/internal/artifacts/uploads" {
            let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
            let art_id = parsed["artifact_id"]
                .as_str()
                .unwrap_or("art-minted")
                .to_string();
            let chat_id = parsed["chat_id"].as_str().unwrap_or("box").to_string();
            let total_bytes = parsed["bytes"].as_u64().unwrap_or(0);
            let expires_at = now_unix_secs() + 900;
            let store_origin = st.object_store_origin.clone();
            let swarm_origin = st.swarm_origin.clone();

            return Ok(match st.reserve_behavior.clone() {
                ReserveBehavior::SinglePut => resp_json(
                    201,
                    &serde_json::json!({
                        "artifact_id": art_id,
                        "chat_id": chat_id,
                        "multipart": false,
                        "part_size": 128 * 1024 * 1024u64,
                        "urls": [format!("{store_origin}/b2/part-1?X-Amz-Signature=PRESIGNED_SECRET_SINGLE_1")],
                        "expires_at": expires_at,
                        "url": format!("{swarm_origin}/v1/instances/i1/artifacts/{art_id}/content"),
                    }),
                ),
                ReserveBehavior::Multipart { part_size } => {
                    let parts = total_bytes.div_ceil(part_size);
                    let urls: Vec<String> = (1..=parts)
                        .map(|n| {
                            format!(
                                "{store_origin}/b2/part-{n}?X-Amz-Signature=PRESIGNED_SECRET_PART_{n}"
                            )
                        })
                        .collect();
                    resp_json(
                        201,
                        &serde_json::json!({
                            "artifact_id": art_id,
                            "chat_id": chat_id,
                            "multipart": true,
                            "part_size": part_size,
                            "urls": urls,
                            "expires_at": expires_at,
                            "url": format!("{swarm_origin}/v1/instances/i1/artifacts/{art_id}/content"),
                        }),
                    )
                }
                ReserveBehavior::JsonError { status, body } => resp_json(status, &body),
                ReserveBehavior::BareStatus(status) => resp_empty(status),
            });
        }

        if path.ends_with("/urls") {
            let uploaded = st.uploaded_parts.clone();
            let store_origin = st.object_store_origin.clone();
            let part_size = match st.reserve_behavior {
                ReserveBehavior::Multipart { part_size } => part_size,
                _ => 128 * 1024 * 1024,
            };
            let urls: Vec<String> = (1..=8)
                .map(|n| {
                    format!(
                        "{store_origin}/b2/part-{n}?X-Amz-Signature=PRESIGNED_SECRET_RESIGNED_{n}"
                    )
                })
                .collect();
            return Ok(resp_json(
                200,
                &serde_json::json!({
                    "multipart": true,
                    "part_size": part_size,
                    "urls": urls,
                    "uploaded_parts": uploaded,
                    "expires_at": now_unix_secs() + 3600,
                }),
            ));
        }

        if path.ends_with("/complete") {
            let art_id = path
                .trim_end_matches("/complete")
                .rsplit('/')
                .next()
                .unwrap_or("a1");
            return Ok(resp_json(
                200,
                &serde_json::json!({
                    "artifact_id": art_id,
                    "state": "ready",
                    "bytes": 1,
                    "url": format!("{}/v1/instances/i1/artifacts/{art_id}/content", st.swarm_origin),
                }),
            ));
        }

        Ok(resp_empty(404))
    }

    /// Minimal `tracing_subscriber::Layer` that records every event message and
    /// field value so tests can assert both what IS logged (error codes) and
    /// what is NEVER logged (tokens, presigned URLs).
    #[derive(Clone, Default)]
    struct LogCapture {
        lines: Arc<Mutex<Vec<String>>>,
    }

    impl LogCapture {
        fn snapshot(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }

    struct FieldRecorder(String);

    impl tracing::field::Visit for FieldRecorder {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut rec = FieldRecorder(format!("{}", event.metadata().level()));
            event.record(&mut rec);
            self.lines.lock().unwrap().push(rec.0);
        }
    }

    #[tokio::test]
    async fn at_or_under_threshold_takes_legacy_ingest_with_no_reserve() {
        let mock = MockHarness::start(ReserveBehavior::SinglePut, PutBehavior::Ok).await;
        let payload = vec![b'x'; INLINE_THRESHOLD_BYTES as usize]; // exact 256 KiB boundary
        push_artefact(
            Some(mock.ingest_cfg()),
            Some(mock.export_cfg()),
            ExportSource::Bytes(Bytes::from(payload.clone())),
            test_meta("a1", "small.bin"),
            fast_policy(),
            None,
        )
        .await;

        let st = mock.state.lock().unwrap();
        assert_eq!(st.swarm_requests.len(), 1, "only one ingest POST");
        assert_eq!(st.swarm_requests[0].path, "/v1/internal/ingest/artefact");
        assert_eq!(
            st.swarm_requests[0]
                .headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer it_11111111111111111111111111111111"),
        );
        assert!(st.store_requests.is_empty(), "no object-store PUT");
    }

    #[tokio::test]
    async fn just_over_threshold_takes_export_protocol_with_right_fields_and_bearer() {
        let mock = MockHarness::start(ReserveBehavior::SinglePut, PutBehavior::Ok).await;
        let payload = vec![0x42u8; (INLINE_THRESHOLD_BYTES + 1) as usize];
        push_artefact(
            Some(mock.ingest_cfg()),
            Some(mock.export_cfg()),
            ExportSource::Bytes(Bytes::from(payload.clone())),
            test_meta("a7", "clip.mp4"),
            fast_policy(),
            None,
        )
        .await;

        let st = mock.state.lock().unwrap();
        assert_eq!(st.swarm_requests.len(), 2, "reserve + complete");
        let reserve = &st.swarm_requests[0];
        assert_eq!(reserve.method, "POST");
        assert_eq!(reserve.path, "/v1/internal/artifacts/uploads");
        assert_eq!(
            reserve.headers.get("authorization").map(String::as_str),
            Some("Bearer pt_secret_token_999999999999999999999999"),
        );
        let body: serde_json::Value = serde_json::from_slice(&reserve.body).unwrap();
        assert_eq!(body["artifact_id"], "a7");
        assert_eq!(body["chat_id"], "c-0001");
        assert_eq!(body["kind"], "other");
        assert_eq!(body["title"], "clip.mp4");
        assert_eq!(body["filename"], "clip.mp4");
        assert_eq!(body["mime"], "video/mp4");
        assert_eq!(body["bytes"], INLINE_THRESHOLD_BYTES + 1);

        assert_eq!(st.store_requests.len(), 1);
        let put = &st.store_requests[0];
        assert_eq!(put.method, "PUT");
        assert_eq!(put.body, payload);
        assert_eq!(
            put.headers.get("content-length").map(String::as_str),
            Some((INLINE_THRESHOLD_BYTES + 1).to_string().as_str())
        );
        assert!(
            !put.headers.contains_key("authorization"),
            "presigned PUT must never carry Authorization"
        );
        assert!(
            !put.headers.contains_key(workload_identity::HEADER),
            "presigned PUT must never carry workload identity"
        );
        assert!(
            !put.headers.contains_key("content-type"),
            "presigned PUT sends only bytes + Content-Length"
        );

        let complete = &st.swarm_requests[1];
        assert_eq!(complete.method, "POST");
        assert_eq!(complete.path, "/v1/internal/artifacts/uploads/a7/complete");
        assert!(complete.body.is_empty());
        assert_eq!(
            complete.headers.get("authorization").map(String::as_str),
            Some("Bearer pt_secret_token_999999999999999999999999"),
        );
    }

    #[tokio::test]
    async fn multipart_uploads_every_part_in_order_from_disk_and_completes() {
        let part_size = 100 * 1024u64;
        let mock =
            MockHarness::start(ReserveBehavior::Multipart { part_size }, PutBehavior::Ok).await;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("demo.mp4");
        // 320 KiB -> 4 parts: 100K, 100K, 100K, 20K (distinct byte per part).
        let mut expected = Vec::with_capacity(320 * 1024);
        expected.extend(std::iter::repeat_n(0x11u8, 100 * 1024));
        expected.extend(std::iter::repeat_n(0x22u8, 100 * 1024));
        expected.extend(std::iter::repeat_n(0x33u8, 100 * 1024));
        expected.extend(std::iter::repeat_n(0x44u8, 20 * 1024));
        std::fs::write(&file_path, &expected).unwrap();

        let outcome = export_to_swarm(
            &mock.export_cfg(),
            &ExportSource::DiskFile {
                path: file_path,
                len: expected.len() as u64,
            },
            &test_meta("a9", "demo.mp4"),
            &fast_policy(),
        )
        .await;
        assert_eq!(outcome, ExportOutcome::Exported);

        let st = mock.state.lock().unwrap();
        assert_eq!(st.store_requests.len(), 4);
        let paths: Vec<&str> = st.store_requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/b2/part-1", "/b2/part-2", "/b2/part-3", "/b2/part-4"]
        );
        let mut reassembled = Vec::new();
        for req in &st.store_requests {
            assert_eq!(
                req.headers.get("content-length").map(String::as_str),
                Some(req.body.len().to_string().as_str())
            );
            reassembled.extend_from_slice(&req.body);
        }
        assert_eq!(reassembled, expected);
        assert_eq!(st.swarm_requests.len(), 2);
        assert_eq!(
            st.swarm_requests[1].path,
            "/v1/internal/artifacts/uploads/a9/complete"
        );
    }

    #[tokio::test]
    async fn artifact_exists_is_treated_as_success() {
        let mock = MockHarness::start(
            ReserveBehavior::JsonError {
                status: 409,
                body: serde_json::json!({
                    "error": "artifact already exists",
                    "code": "artifact_exists"
                }),
            },
            PutBehavior::Ok,
        )
        .await;
        let payload = vec![b'a'; (INLINE_THRESHOLD_BYTES + 16) as usize];
        let outcome = export_to_swarm(
            &mock.export_cfg(),
            &ExportSource::Bytes(Bytes::from(payload)),
            &test_meta("a5", "dup.bin"),
            &fast_policy(),
        )
        .await;
        assert_eq!(outcome, ExportOutcome::AlreadyExists);
        let st = mock.state.lock().unwrap();
        assert_eq!(st.swarm_requests.len(), 1, "no retry and no complete");
        assert!(st.store_requests.is_empty());
    }

    #[tokio::test]
    async fn artifacts_disabled_and_404_fall_back_when_body_fits_and_fail_above_25mib() {
        for (name, reserve_behavior) in [
            (
                "artifacts_disabled",
                ReserveBehavior::JsonError {
                    status: 501,
                    body: serde_json::json!({
                        "error": "disabled",
                        "code": "artifacts_disabled"
                    }),
                },
            ),
            ("404", ReserveBehavior::BareStatus(404)),
        ] {
            // Case 1: body > 256 KiB and <= 25 MiB -> falls back to legacy base64 ingest.
            let mock = MockHarness::start(reserve_behavior.clone(), PutBehavior::Ok).await;
            let payload = vec![b'z'; (INLINE_THRESHOLD_BYTES + 64) as usize];
            push_artefact(
                Some(mock.ingest_cfg()),
                Some(mock.export_cfg()),
                ExportSource::Bytes(Bytes::from(payload)),
                test_meta("a2", "fits.bin"),
                fast_policy(),
                None,
            )
            .await;
            {
                let st = mock.state.lock().unwrap();
                assert_eq!(
                    st.swarm_requests.len(),
                    2,
                    "{name}: reserve + fallback ingest"
                );
                assert_eq!(st.swarm_requests[0].path, "/v1/internal/artifacts/uploads");
                assert_eq!(st.swarm_requests[1].path, "/v1/internal/ingest/artefact");
                assert_eq!(
                    st.swarm_requests[1]
                        .headers
                        .get("authorization")
                        .map(String::as_str),
                    Some("Bearer it_11111111111111111111111111111111"),
                );
            }

            // Case 2: body > 25 MiB -> does NOT call legacy ingest; fails cleanly & logs warn.
            let mock_big = MockHarness::start(reserve_behavior, PutBehavior::Ok).await;
            let dir = tempfile::tempdir().unwrap();
            let big_path = dir.path().join("huge.mp4");
            let big_len = MAX_LEGACY_INGEST_BYTES + 1024;
            let f = std::fs::File::create(&big_path).unwrap();
            f.set_len(big_len).unwrap();
            drop(f);

            let (tx, mut rx) = broadcast::channel(8);
            let logs = LogCapture::default();
            let sub = tracing_subscriber::registry().with(logs.clone());
            push_artefact(
                Some(mock_big.ingest_cfg()),
                Some(mock_big.export_cfg()),
                ExportSource::DiskFile {
                    path: big_path,
                    len: big_len,
                },
                test_meta("a3", "huge.mp4"),
                fast_policy(),
                Some(tx),
            )
            .with_subscriber(sub)
            .await;

            {
                let st = mock_big.state.lock().unwrap();
                assert_eq!(
                    st.swarm_requests.len(),
                    1,
                    "{name}: over-25MiB body must NOT attempt legacy ingest"
                );
            }
            let notice = match rx.try_recv().unwrap() {
                SseEvent::Text { delta } => delta,
                _ => panic!("expected text failure notice"),
            };
            assert!(notice.contains("too large"), "{name}: {notice}");
            let captured = logs.snapshot().join("\n");
            assert!(
                captured.contains("WARN"),
                "{name}: expected warn log, got {captured}"
            );
        }
    }

    #[tokio::test]
    async fn non_retryable_rejections_do_not_retry_and_are_reported() {
        let cases = [
            (
                "quota_exceeded",
                507u16,
                serde_json::json!({
                    "error": "owner quota exceeded",
                    "code": "quota_exceeded",
                    "quota_bytes": 1000,
                    "used_bytes": 950
                }),
            ),
            (
                "object_too_large",
                413u16,
                serde_json::json!({
                    "error": "file exceeds max object size",
                    "code": "object_too_large",
                    "max_object_bytes": 500
                }),
            ),
            (
                "forbidden",
                403u16,
                serde_json::json!({
                    "error": "not allowed",
                    "code": "forbidden"
                }),
            ),
        ];

        for (code, status, body) in cases {
            let mock =
                MockHarness::start(ReserveBehavior::JsonError { status, body }, PutBehavior::Ok)
                    .await;
            let (tx, mut rx) = broadcast::channel(8);
            let logs = LogCapture::default();
            let sub = tracing_subscriber::registry().with(logs.clone());

            push_artefact(
                Some(mock.ingest_cfg()),
                Some(mock.export_cfg()),
                ExportSource::Bytes(Bytes::from(vec![
                    b'q';
                    (INLINE_THRESHOLD_BYTES + 1) as usize
                ])),
                test_meta("a4", "denied.mp4"),
                fast_policy(),
                Some(tx),
            )
            .with_subscriber(sub)
            .await;

            let st = mock.state.lock().unwrap();
            assert_eq!(
                st.swarm_requests.len(),
                1,
                "{code} must NOT retry reserve or fall back to ingest"
            );
            assert!(st.store_requests.is_empty());

            let captured = logs.snapshot().join("\n");
            assert!(
                captured.contains("WARN") && captured.contains(code),
                "{code}: warn log must include error code; got: {captured}"
            );
            let notice = match rx.try_recv().unwrap() {
                SseEvent::Text { delta } => delta,
                _ => panic!("expected text notice for {code}"),
            };
            assert!(notice.contains(code), "{code}: notice={notice}");
        }
    }

    #[tokio::test]
    async fn transient_put_failure_is_retried_and_succeeds() {
        let mock = MockHarness::start(
            ReserveBehavior::SinglePut,
            PutBehavior::FailFirst {
                failures: 2,
                status: 503,
            },
        )
        .await;
        let payload = vec![0x7Au8; (INLINE_THRESHOLD_BYTES + 128) as usize];
        let outcome = export_to_swarm(
            &mock.export_cfg(),
            &ExportSource::Bytes(Bytes::from(payload.clone())),
            &test_meta("a8", "retry.mp4"),
            &fast_policy(),
        )
        .await;
        assert_eq!(outcome, ExportOutcome::Exported);

        let st = mock.state.lock().unwrap();
        assert_eq!(st.store_requests.len(), 3, "2 transient 503s + 1 success");
        assert_eq!(st.store_requests[2].body, payload);
        assert_eq!(st.swarm_requests.len(), 2, "reserve + complete");
    }

    #[tokio::test]
    async fn expired_presigned_url_triggers_resign_and_resumes_multipart() {
        let part_size = 100 * 1024u64;
        let mock = MockHarness::start(
            ReserveBehavior::Multipart { part_size },
            PutBehavior::ExpirePartOnce { part_num: 2 },
        )
        .await;
        let payload = vec![0x55u8; 280 * 1024]; // 3 parts
        let outcome = export_to_swarm(
            &mock.export_cfg(),
            &ExportSource::Bytes(Bytes::from(payload)),
            &test_meta("a10", "resume.mp4"),
            &fast_policy(),
        )
        .await;
        assert_eq!(outcome, ExportOutcome::Exported);

        let st = mock.state.lock().unwrap();
        let swarm_paths: Vec<&str> = st.swarm_requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            swarm_paths,
            vec![
                "/v1/internal/artifacts/uploads",
                "/v1/internal/artifacts/uploads/a10/urls",
                "/v1/internal/artifacts/uploads/a10/complete",
            ]
        );
        // Part 1 succeeded, part 2 got 403 -> re-signed -> part 2 retried with resigned URL -> part 3.
        assert_eq!(st.store_requests.len(), 4);
        assert!(
            st.store_requests[2]
                .query
                .as_deref()
                .unwrap_or("")
                .contains("RESIGNED_2")
        );
    }

    #[tokio::test]
    async fn total_timeout_bounds_stalled_upload() {
        let mock = MockHarness::start(ReserveBehavior::SinglePut, PutBehavior::Hang).await;
        let policy = ExportRetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            total_timeout: Duration::from_millis(200),
            control_timeout: Duration::from_secs(2),
            put_timeout: Duration::from_secs(2),
        };
        let started = Instant::now();
        let outcome = export_to_swarm(
            &mock.export_cfg(),
            &ExportSource::Bytes(Bytes::from(vec![
                1u8;
                (INLINE_THRESHOLD_BYTES + 1) as usize
            ])),
            &test_meta("a11", "hang.mp4"),
            &policy,
        )
        .await;
        assert!(matches!(outcome, ExportOutcome::Failed { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "total_timeout must bound stalled upload; took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn token_and_presigned_urls_never_appear_in_captured_logs_or_notices() {
        let logs = LogCapture::default();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(logs.clone()));

        let export_cfg = ExportConfig {
            origin: String::new(),
            token: "pt_super_secret_token_0123456789abcdef".to_string(),
        };

        // Scenario 1: fatal PUT 503 failure (exercises retry + failure logging).
        let mock_fail =
            MockHarness::start(ReserveBehavior::SinglePut, PutBehavior::AlwaysStatus(503)).await;
        let cfg1 = ExportConfig {
            origin: mock_fail.swarm_origin.clone(),
            token: export_cfg.token.clone(),
        };
        let (tx, mut rx) = broadcast::channel(16);
        push_artefact(
            None,
            Some(cfg1),
            ExportSource::Bytes(Bytes::from(vec![
                b'k';
                (INLINE_THRESHOLD_BYTES + 1) as usize
            ])),
            test_meta("a12", "secret-test.mp4"),
            fast_policy(),
            Some(tx.clone()),
        )
        .with_subscriber(dispatch.clone())
        .await;

        // Scenario 2: reserve network error against dead port with secret token.
        let cfg2 = ExportConfig {
            origin: "http://127.0.0.1:1".to_string(),
            token: export_cfg.token.clone(),
        };
        push_artefact(
            None,
            Some(cfg2),
            ExportSource::Bytes(Bytes::from(vec![
                b'k';
                (INLINE_THRESHOLD_BYTES + 1) as usize
            ])),
            test_meta("a13", "dead-port.mp4"),
            fast_policy(),
            Some(tx),
        )
        .with_subscriber(dispatch)
        .await;

        let mut combined = logs.snapshot().join("\n");
        while let Ok(evt) = rx.try_recv() {
            if let SseEvent::Text { delta } = evt {
                combined.push('\n');
                combined.push_str(&delta);
            }
        }
        assert!(
            !combined.is_empty(),
            "expected captured warning logs and failure notices"
        );
        assert!(
            !combined.contains("pt_super_secret_token_0123456789abcdef"),
            "proxy token leaked into logs/notices: {combined}"
        );
        assert!(
            !combined.contains("PRESIGNED_SECRET"),
            "presigned URL signature leaked into logs/notices: {combined}"
        );
        assert!(
            !combined.contains("X-Amz-Signature"),
            "presigned URL query leaked into logs/notices: {combined}"
        );

        // Also check Debug output redaction directly.
        let dbg_cfg = format!("{export_cfg:?}");
        assert!(!dbg_cfg.contains(&export_cfg.token));
        let dbg_url = format!(
            "{:?}",
            PresignedUrl("https://b2.example/x?sig=SECRET".into())
        );
        assert!(!dbg_url.contains("SECRET"));
        assert!(!dbg_url.contains("b2.example"));
    }

    #[test]
    fn swarm_post_attaches_workload_identity_header_for_pt_token() {
        // Isolate DYSON_WORKLOAD_IDENTITY_FILE in a subprocess so parallel
        // tests never race on process environment variables.
        const CHILD: &str = "DYSON_EXPORT_IDENTITY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = std::thread::current().name().unwrap().to_owned();
            let dir = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test])
                .env(CHILD, "1")
                .env("DYSON_WORKLOAD_IDENTITY_FILE", dir.path().join("token"))
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let path = std::env::var_os("DYSON_WORKLOAD_IDENTITY_FILE").unwrap();
        std::fs::write(&path, "jwt-workload-identity-token").unwrap();
        let req = swarm_post(
            "https://swarm.example/v1/internal/artifacts/uploads",
            "pt_12345678901234567890123456789012",
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers()[workload_identity::HEADER],
            "jwt-workload-identity-token"
        );
        assert_eq!(
            req.headers()[reqwest::header::AUTHORIZATION],
            "Bearer pt_12345678901234567890123456789012"
        );
    }

    #[test]
    fn contract_field_sanitizers_enforce_limits_and_forbidden_chars() {
        assert_eq!(
            sanitize_contract_filename("foo/bar\\baz\0\n.mp4", "a1"),
            "foobarbaz.mp4"
        );
        assert_eq!(sanitize_contract_filename("///", "a1"), "a1.bin");
        let long = "a".repeat(400);
        assert_eq!(sanitize_contract_filename(&long, "a1").len(), 255);
        assert_eq!(sanitize_contract_title(&long).unwrap().len(), 200);
        assert_eq!(
            sanitize_contract_mime("").as_str(),
            "application/octet-stream"
        );
        assert_eq!(sanitize_contract_mime(&long).len(), 127);
        assert_eq!(sanitize_contract_kind(&long).len(), 64);
    }
}
