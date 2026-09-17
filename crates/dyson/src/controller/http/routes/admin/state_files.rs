//! Restore allowlisted durable state from Swarm.
use super::{HttpState, Resp, authorize_configure, bad_request, json_ok, read_json_capped};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use hyper::Request;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// State replay carries base64 file bodies.  The sync worker caps source
/// files at 5 MiB, so 8 MiB leaves JSON/base64 headroom without turning
/// the admin surface into a bulk upload endpoint.
const MAX_STATE_FILE_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct RestoreStateFileBody {
    namespace: String,
    path: String,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    body_b64: Option<String>,
}

pub(in super::super) async fn post_state_file(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    let body: RestoreStateFileBody = match read_json_capped(req, MAX_STATE_FILE_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let root = match state_root(&snapshot, &body.namespace) {
        Ok(root) => root,
        Err(e) => return bad_request(&e),
    };
    let rel = match clean_relative_path(&body.path) {
        Ok(path) => path,
        Err(e) => return bad_request(&e),
    };
    let rel_path = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if !crate::swarm_state_sync::is_durable_state_file_path(&body.namespace, &rel_path) {
        return bad_request("state file path is not durable state");
    }
    let abs = root.join(&rel);

    if body.deleted {
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return bad_request(&format!("remove {}: {e}", abs.display())),
        }
        return json_ok(&serde_json::json!({
            "ok": true,
            "namespace": body.namespace,
            "path": body.path,
            "deleted": true,
        }));
    }

    let Some(encoded) = body.body_b64.as_deref() else {
        return bad_request("body_b64 is required unless deleted=true");
    };
    let bytes = match B64.decode(encoded) {
        Ok(bytes) => bytes,
        Err(e) => return bad_request(&format!("body_b64 decode: {e}")),
    };
    if bytes.len() > 5 * 1024 * 1024 {
        return bad_request("state file exceeds 5 MiB");
    }
    if crate::swarm_state_sync::is_zero_byte_chat_transcript(
        &body.namespace,
        &rel_path,
        bytes.len() as u64,
    ) {
        return bad_request("zero-byte chat transcripts are not durable state");
    }
    if let Some(parent) = abs.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return bad_request(&format!("mkdir {}: {e}", parent.display()));
    }
    let tmp = abs.with_extension("dyson-state-restore.tmp");
    if let Err(e) = tokio::fs::write(&tmp, &bytes).await {
        return bad_request(&format!("write {}: {e}", tmp.display()));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &abs).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return bad_request(&format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            abs.display()
        ));
    }
    state.observe_replayed_state_file(&body.namespace, &rel_path);
    json_ok(&serde_json::json!({
        "ok": true,
        "namespace": body.namespace,
        "path": body.path,
        "mime": body.mime,
        "bytes": bytes.len(),
        "deleted": false,
    }))
}

fn state_root(
    settings: &crate::config::Settings,
    namespace: &str,
) -> std::result::Result<PathBuf, String> {
    match namespace {
        "workspace" => Ok(crate::util::resolve_tilde(
            settings.workspace.connection_string.expose(),
        )),
        "chats" => Ok(crate::util::resolve_tilde(
            settings.chat_history.connection_string.expose(),
        )),
        _ => Err(format!("unsupported namespace {namespace:?}")),
    }
}

pub(super) fn clean_relative_path(path: &str) -> std::result::Result<PathBuf, String> {
    if path.is_empty() || path.len() > 2048 || path.contains('\0') {
        return Err("bad path length or nul byte".into());
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err("paths must be relative and slash-separated".into());
    }
    let p = Path::new(path);
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => out.push(part),
            _ => return Err("paths must be clean relative paths".into()),
        }
    }
    if out.as_os_str().is_empty() {
        return Err("path is empty".into());
    }
    Ok(out)
}
