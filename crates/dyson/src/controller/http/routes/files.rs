// ===========================================================================
// /api/files/:id — serve agent-produced bytes from the in-memory FIFO
// cache, falling back to disk for entries that have aged out.
// ===========================================================================

use hyper::{Response, StatusCode};

use super::super::responses::{Resp, boxed, not_found, safe_store_id, sanitize_filename};
use super::super::state::HttpState;
use super::super::stores::{FileEntry, FileStore};

/// Serve a previously-stored agent-produced file (image, PoC, etc.) scoped to
/// its owning chat.
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
    file_response(entry)
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
        store.items.get(id).map(|e| FileEntry {
            bytes: e.bytes.clone(),
            mime: e.mime.clone(),
            name: e.name.clone(),
            chat_id: e.chat_id.clone(),
        })
    };
    Some(match cached {
        Some(t) => t,
        None => {
            let loaded = state
                .data_dir
                .as_ref()
                .and_then(|dir| FileStore::load_from_disk(dir, id));
            match loaded {
                Some(e) => {
                    // Warm the cache so subsequent hits don't re-read
                    // disk for the same id (browser preview, repeated
                    // downloads, etc.).  Recover from poisoning so a
                    // panicked previous holder doesn't silently disable
                    // the cache.
                    let out = FileEntry {
                        bytes: e.bytes.clone(),
                        mime: e.mime.clone(),
                        name: e.name.clone(),
                        chat_id: e.chat_id.clone(),
                    };
                    let mut s = match state.files.lock() {
                        Ok(s) => s,
                        Err(p) => p.into_inner(),
                    };
                    s.put(id.to_string(), e);
                    drop(s);
                    out
                }
                None => return None,
            }
        }
    })
}

fn file_response(entry: FileEntry) -> Resp {
    // sanitize_filename strips `\r`, `\n`, `"`, `/`, `\\` — protects
    // the Content-Disposition header from CRLF injection if a tool
    // ever produced a maliciously-shaped filename.  The previous
    // shape only stripped `"`, leaving CRLF unguarded.
    let safe = sanitize_filename(&entry.name);
    let cd = if entry.mime.starts_with("image/") {
        format!("inline; filename=\"{safe}\"")
    } else {
        format!("attachment; filename=\"{safe}\"")
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", entry.mime)
        .header("Content-Disposition", cd)
        .header("Cache-Control", "no-cache")
        .body(boxed(hyper::body::Bytes::from(entry.bytes)))
        .unwrap()
}
