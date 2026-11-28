// ===========================================================================
// /api/conversations/<chat>/files/:id — serve agent-produced bytes from the in-memory FIFO
// cache, falling back to disk for entries that have aged out.
// ===========================================================================

use std::convert::Infallible;

use http_body_util::{BodyExt, StreamBody, combinators::BoxBody};
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::super::responses::{Resp, boxed, not_found, safe_store_id, sanitize_filename};
use super::super::state::HttpState;
use super::super::stores::{FileBody, FileEntry, FileStore};

/// Serve a previously-stored agent-produced file (image, PoC, video, etc.)
/// scoped to its owning chat.
pub(super) async fn get_for_chat(state: &HttpState, chat_id: &str, id: &str) -> Resp {
    // Reject anything outside the minted alphabet (dispatch hands us
    // the URL-decoded value; an attacker submitting `%2F../etc/passwd`
    // would otherwise traverse).  Mint-only ids are `f<u64>`.
    if !safe_store_id(chat_id) || !safe_store_id(id) {
        return not_found();
    }
    let Some(entry) = load_entry(state, id) else {
        return not_found();
    };
    if entry.chat_id != chat_id {
        return not_found();
    }
    file_response(entry).await
}

fn load_entry(state: &HttpState, id: &str) -> Option<FileEntry> {
    // Check the in-memory cache first, then fall back to disk.  Files
    // evicted from the FIFO cache stay reachable as long as the
    // controller has a data_dir configured — which is always true when
    // the operator has chat_history on.
    let cached = {
        let store = match state.files.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store.items.get(id).cloned()
    };
    if let Some(entry) = cached {
        return Some(entry);
    }

    let entry = state
        .data_dir
        .as_ref()
        .and_then(|dir| FileStore::load_from_disk(dir, id))?;

    // Warm the cache so subsequent hits don't re-read disk for the same id
    // (browser preview, repeated downloads, etc.). Recover from poisoning so
    // a panicked previous holder doesn't silently disable the cache.
    let response = entry.clone();
    let mut store = match state.files.lock() {
        Ok(store) => store,
        Err(poisoned) => poisoned.into_inner(),
    };
    store.put(id.to_string(), entry);
    drop(store);
    Some(response)
}

fn boxed_file_stream(mut file: tokio::fs::File) -> BoxBody<Bytes, Infallible> {
    let stream = async_stream::stream! {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
                Ok(0) => break,
                Ok(n) => yield Ok(hyper::body::Frame::data(Bytes::copy_from_slice(&buf[..n]))),
                Err(e) => {
                    tracing::warn!(error = %e, "file stream read failed");
                    break;
                }
            }
        }
    };
    StreamBody::new(stream).boxed()
}

async fn file_response(entry: FileEntry) -> Resp {
    let (body, content_length) = match entry.body {
        FileBody::Memory(bytes) => {
            let len = bytes.len() as u64;
            (boxed(Bytes::from(bytes)), len)
        }
        FileBody::Disk { path, len } => match tokio::fs::File::open(&path).await {
            Ok(file) => {
                let actual_len = file.metadata().await.map(|m| m.len()).unwrap_or(len);
                (boxed_file_stream(file), actual_len)
            }
            Err(_) => return not_found(),
        },
    };
    file_response_with_body(body, content_length, &entry.mime, &entry.name)
}

fn file_response_with_body(
    body: BoxBody<Bytes, Infallible>,
    content_length: u64,
    mime: &str,
    name: &str,
) -> Resp {
    // sanitize_filename strips `\r`, `\n`, `"`, `/`, `\\` — protects
    // the Content-Disposition header from CRLF injection if a tool
    // ever produced a maliciously-shaped filename.  The previous
    // shape only stripped `"`, leaving CRLF unguarded.
    let safe = sanitize_filename(name);
    // Raster images preview inline.  SVG is a document that can carry
    // script, so it is downloaded rather than rendered on this origin.
    let inline = mime.starts_with("image/") && !mime.starts_with("image/svg");
    let cd = if inline {
        format!("inline; filename=\"{safe}\"")
    } else {
        format!("attachment; filename=\"{safe}\"")
    };
    // The MIME string comes from the producing tool; an invalid header
    // value must degrade to an opaque download, not panic the handler.
    let content_type = hyper::header::HeaderValue::try_from(mime).unwrap_or(
        hyper::header::HeaderValue::from_static("application/octet-stream"),
    );
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    let headers = resp.headers_mut();
    headers.insert(hyper::header::CONTENT_TYPE, content_type);
    if let Ok(cl) = hyper::header::HeaderValue::try_from(content_length.to_string()) {
        headers.insert(hyper::header::CONTENT_LENGTH, cl);
    }
    if let Ok(cd) = hyper::header::HeaderValue::try_from(cd) {
        headers.insert(hyper::header::CONTENT_DISPOSITION, cd);
    }
    headers.insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mime: &str, name: &str) -> FileEntry {
        FileEntry {
            body: FileBody::Memory(b"<svg onload=alert(1)/>".to_vec()),
            mime: mime.to_string(),
            name: name.to_string(),
            chat_id: "c-0001".to_string(),
        }
    }

    #[tokio::test]
    async fn svg_is_served_as_attachment() {
        // SEC-22: an inline SVG navigated to directly runs its scripts on
        // this origin.
        let resp = file_response(entry("image/svg+xml", "x.svg")).await;
        let cd = resp.headers()["content-disposition"].to_str().unwrap();
        assert!(cd.starts_with("attachment"), "{cd}");
        // Raster images still preview inline.
        let resp = file_response(entry("image/png", "x.png")).await;
        let cd = resp.headers()["content-disposition"].to_str().unwrap();
        assert!(cd.starts_with("inline"), "{cd}");
    }

    #[tokio::test]
    async fn invalid_mime_does_not_panic() {
        let resp = file_response(entry("text/plain\r\nX-Evil: 1", "x.txt")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["content-type"].to_str().unwrap(),
            "application/octet-stream"
        );
        assert!(resp.headers().get("x-evil").is_none());
    }

    #[tokio::test]
    async fn disk_backed_entry_streams_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video.mp4");
        let payload = vec![0xAB; 150 * 1024];
        std::fs::write(&path, &payload).unwrap();
        let resp = file_response(FileEntry {
            body: FileBody::Disk {
                path,
                len: payload.len() as u64,
            },
            mime: "video/mp4".to_string(),
            name: "video.mp4".to_string(),
            chat_id: "c-0001".to_string(),
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["content-length"].to_str().unwrap(),
            payload.len().to_string()
        );
        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(collected.as_ref(), payload.as_slice());
    }
}
