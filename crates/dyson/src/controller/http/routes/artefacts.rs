// ===========================================================================
// Artefact endpoints —
//   GET /api/artefacts/:id                     — body + meta
//   GET /api/conversations/:id/artefacts        — list per chat
//   GET /api/conversations/:id/export           — ShareGPT dump
// ===========================================================================

use hyper::{Response, StatusCode};

use super::super::responses::{Resp, bad_request, boxed, json_ok, not_found, safe_store_id, sanitize_filename};
use super::super::state::HttpState;
use super::super::stores::ArtefactStore;
use super::super::wire::ArtefactDto;

/// Serve the raw markdown body of a stored artefact.  The client-side
/// renderer in `turns.jsx` turns it into HTML; we just hand over the
/// bytes with the right mime type so "copy" / "download" on the reader
/// get what they expect.  Returns 404 when the FIFO has evicted the
/// entry (expected after ~32 reports on a long session) — the UI shows
/// a "no longer in memory — rerun to regenerate" fallback.
pub(super) async fn get(state: &HttpState, id: &str) -> Resp {
    if !safe_store_id(id) {
        return not_found();
    }
    let cached = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store.items.get(id).map(|e| {
            (
                e.content.clone().into_bytes(),
                e.mime_type.clone(),
                e.title.clone(),
                e.chat_id.clone(),
            )
        })
    };
    let (bytes, mime, title, chat_id) = match cached {
        Some(t) => t,
        None => {
            let loaded = state
                .data_dir
                .as_ref()
                .and_then(|dir| ArtefactStore::load_from_disk(dir, id));
            match loaded {
                Some(e) => {
                    let out = (
                        e.content.clone().into_bytes(),
                        e.mime_type.clone(),
                        e.title.clone(),
                        e.chat_id.clone(),
                    );
                    if let Ok(mut s) = state.artefacts.lock() {
                        s.put(id.to_string(), e);
                    }
                    out
                }
                None => return not_found(),
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", format!("{mime}; charset=utf-8"))
        .header(
            "Content-Disposition",
            format!("inline; filename=\"{}.md\"", sanitize_filename(&title)),
        )
        // Surfaces the owning chat to the SPA so a direct deep-link
        // (`/#/artefacts/<id>` opened cold) can restore the sidebar
        // context without a second round-trip.
        .header("X-Dyson-Chat-Id", chat_id)
        .header("Cache-Control", "no-cache")
        .body(boxed(hyper::body::Bytes::from(bytes)))
        .unwrap()
}

/// Stream a ShareGPT-format dump of a conversation for the web UI's
/// download button.  Reads the transcript from `ChatHistory` (or the
/// in-memory agent's messages if history is absent), folds in the
/// per-turn feedback ratings, and serialises via the same
/// `sharegpt::to_sharegpt_with_feedback` path the `export_conversation`
/// tool uses.  Returns `{"error":..}` JSON on 404 so the bridge can
/// surface the message inline.
pub(super) async fn export(state: &HttpState, chat_id: &str) -> Resp {
    // Transcript: prefer disk (authoritative, has everything ever sent
    // for this chat) and fall back to the live agent's in-memory
    // message buffer when no history backend is configured.
    let messages = if let Some(h) = state.history.as_ref() {
        match h.load(chat_id) {
            Ok(m) => m,
            Err(e) => return bad_request(&format!("load transcript: {e}")),
        }
    } else {
        let chats = state.chats.lock().await;
        let Some(handle) = chats.get(chat_id) else {
            return not_found();
        };
        let guard = handle.agent.lock().await;
        match guard.as_ref() {
            Some(a) => a.messages().to_vec(),
            None => Vec::new(),
        }
    };
    if messages.is_empty() {
        return not_found();
    }

    // System prompt mirrors the behaviour of the in-tree tool — use
    // the live agent's current prompt when available so exports
    // capture the persona/role the chat was actually run with.
    let system_prompt: Option<String> = {
        let chats = state.chats.lock().await;
        let handle = chats.get(chat_id).cloned();
        drop(chats);
        if let Some(h) = handle {
            let guard = h.agent.lock().await;
            guard.as_ref().map(|a| a.system_prompt().to_string())
        } else {
            None
        }
    };

    let feedback = state
        .feedback
        .as_ref()
        .and_then(|f| f.load(chat_id).ok())
        .unwrap_or_default();

    let convo = crate::export::sharegpt::to_sharegpt_with_feedback(
        &messages,
        system_prompt.as_deref(),
        Some(chat_id.to_string()),
        &feedback,
    );
    let body = match crate::export::sharegpt::to_sharegpt_json(&[convo]) {
        Ok(s) => s,
        Err(e) => return bad_request(&format!("serialise sharegpt: {e}")),
    };

    let filename = format!("{chat_id}.sharegpt.json");
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", sanitize_filename(&filename)),
        )
        .header("Cache-Control", "no-cache")
        .body(boxed(hyper::body::Bytes::from(body)))
        .unwrap()
}

pub(super) async fn list(state: &HttpState, chat_id: &str) -> Resp {
    let items: Vec<ArtefactDto> = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        // Walk `order` back-to-front so the newest sit on top.  The
        // FIFO ordering IS creation order — artefacts never reorder,
        // they only evict from the front.
        store
            .order
            .iter()
            .rev()
            .filter_map(|id| store.items.get(id).map(|e| (id, e)))
            .filter(|(_, e)| e.chat_id == chat_id)
            .map(|(id, e)| ArtefactDto {
                id: id.clone(),
                kind: e.kind,
                title: e.title.clone(),
                bytes: e.content.len(),
                created_at: e.created_at,
                metadata: e.metadata.clone(),
            })
            .collect()
    };
    json_ok(&items)
}
