// ===========================================================================
// Artefact endpoints —
//   GET /api/conversations/:id/artefacts             — list per chat
//   GET /api/conversations/:id/artefacts/:artefact   — body + meta scoped to chat
//   GET /api/conversations/:id/export                — ShareGPT dump
// ===========================================================================

use hyper::{Response, StatusCode};

use super::super::responses::{
    Resp, bad_request, boxed, json_ok, not_found, safe_store_id, sanitize_filename,
    sanitize_header_value,
};
use super::super::state::HttpState;
use super::super::stores::ArtefactStore;
use super::super::wire::ArtefactDto;

/// Serve an artefact body scoped by chat id.  This is the duplicate-safe
/// form used by the SPA reader and swarm's same-origin share mint path:
/// artefact ids can be reused across restored/imported chat directories, so
/// the tuple `(chat_id, artefact_id)` is the stable identity.
pub(super) async fn get_for_chat(state: &HttpState, chat_id: &str, id: &str) -> Resp {
    if !safe_store_id(chat_id) || !safe_store_id(id) {
        return not_found();
    }
    let loaded = state
        .data_dir
        .as_ref()
        .and_then(|dir| ArtefactStore::load_from_disk_for_chat(dir, chat_id, id));
    let entry = match loaded {
        Some(e) => e,
        None => {
            let cached = {
                let store = match state.artefacts.lock() {
                    Ok(s) => s,
                    Err(p) => p.into_inner(),
                };
                store
                    .items
                    .get(id)
                    .filter(|e| e.chat_id == chat_id)
                    .map(|e| {
                        (
                            e.content.clone().into_bytes(),
                            e.mime_type.clone(),
                            e.title.clone(),
                            e.chat_id.clone(),
                        )
                    })
            };
            let Some((bytes, mime, title, chat_id)) = cached else {
                return not_found();
            };
            return artefact_body_response(bytes, mime, title, chat_id);
        }
    };
    artefact_body_response(
        entry.content.into_bytes(),
        entry.mime_type,
        entry.title,
        entry.chat_id,
    )
}

fn artefact_body_response(bytes: Vec<u8>, mime: String, title: String, chat_id: String) -> Resp {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", format!("{mime}; charset=utf-8"))
        .header(
            "Content-Disposition",
            format!("inline; filename=\"{}.md\"", sanitize_filename(&title)),
        )
        .header("X-Dyson-Chat-Id", sanitize_header_value(&chat_id))
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
    json_ok(&list_for_chat(state, chat_id).await)
}

pub(super) async fn list_for_chat(state: &HttpState, chat_id: &str) -> Vec<ArtefactDto> {
    // Disk is the authoritative source — the in-memory FIFO has a hard
    // cap (`MAX_ARTEFACTS`) and a long-running session that emits more
    // than the cap will evict older entries from the cache, even
    // though their bytes are still on disk.  The previous shape walked
    // only `store.order`, which silently dropped those evicted entries
    // from the listing endpoint and therefore from the sidebar.
    //
    // Walk the chat's `artefacts/` subdir on disk and prefer the
    // in-memory entry for body length when it's still cached (avoids
    // a stat() call per artefact when the cache is warm — typical
    // case).  Memory-only deployments (no `data_dir`) fall through to
    // the cache-only path; the cap there bounds total artefacts so
    // eviction-during-session can't lose anything that isn't also on
    // disk.
    let mut items: Vec<ArtefactDto> = Vec::new();
    if let Some(dir) = state.data_dir.as_ref() {
        let sub = ArtefactStore::dir_for_chat(dir, chat_id);
        let cached: std::collections::HashMap<String, ArtefactDto> = {
            let store = match state.artefacts.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            store
                .items
                .iter()
                .filter(|(_id, entry)| entry.chat_id == chat_id)
                .map(|(id, entry)| {
                    (
                        id.clone(),
                        ArtefactDto {
                            id: id.clone(),
                            kind: entry.kind,
                            title: entry.title.clone(),
                            bytes: entry.content.len(),
                            created_at: entry.created_at,
                            tool_use_id: entry.tool_use_id.clone(),
                            metadata: entry.metadata.clone(),
                        },
                    )
                })
                .collect()
        };
        if let Ok(mut rd) = tokio::fs::read_dir(&sub).await {
            loop {
                let e = match rd.next_entry().await {
                    Ok(Some(e)) => e,
                    Ok(None) | Err(_) => break,
                };
                let name = match e.file_name().into_string() {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let id = match name.strip_suffix(".meta.json") {
                    Some(id) => id.to_string(),
                    None => continue,
                };
                let dto = match cached.get(&id) {
                    Some(entry) => entry.clone(),
                    _ => match read_meta_dto(&sub, &id).await {
                        Some(dto) => dto,
                        None => continue,
                    },
                };
                items.push(dto);
            }
        }
    } else {
        // Memory-only mode — no disk to walk.  Iterate `order`
        // back-to-front so newest-first ordering matches the disk
        // path's post-sort.
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        for id in store.order.iter().rev() {
            if let Some(entry) = store.items.get(id)
                && entry.chat_id == chat_id
            {
                items.push(ArtefactDto {
                    id: id.clone(),
                    kind: entry.kind,
                    title: entry.title.clone(),
                    bytes: entry.content.len(),
                    created_at: entry.created_at,
                    tool_use_id: entry.tool_use_id.clone(),
                    metadata: entry.metadata.clone(),
                });
            }
        }
    }
    // Newest first.  read_dir is unordered, so sort by (created_at,
    // numeric_id) descending — gives a stable order even when two
    // artefacts share a wall-clock second.
    items.sort_by(|a, b| {
        b.created_at.cmp(&a.created_at).then_with(|| {
            let an: u64 =
                a.id.strip_prefix('a')
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            let bn: u64 =
                b.id.strip_prefix('a')
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            bn.cmp(&an)
        })
    });
    items
}

/// Read just enough of an artefact's metadata + body-size to populate
/// an `ArtefactDto`.  Avoids loading the body into memory the way
/// `ArtefactStore::load_from_disk` does — `list_artefacts` doesn't
/// need it, and a chat with hundreds of long reports would otherwise
/// allocate the lot per list call.
async fn read_meta_dto(sub: &std::path::Path, id: &str) -> Option<ArtefactDto> {
    let meta_txt = tokio::fs::read_to_string(sub.join(format!("{id}.meta.json")))
        .await
        .ok()?;
    let meta: serde_json::Value = serde_json::from_str(&meta_txt).ok()?;
    let kind: crate::message::ArtefactKind = meta
        .get("kind")
        .and_then(|k| serde_json::from_value(k.clone()).ok())
        .unwrap_or(crate::message::ArtefactKind::Other);
    let bytes = tokio::fs::metadata(sub.join(format!("{id}.body")))
        .await
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    Some(ArtefactDto {
        id: id.to_string(),
        kind,
        title: meta
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Artefact")
            .to_string(),
        bytes,
        created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
        tool_use_id: meta
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        metadata: meta.get("metadata").cloned().filter(|v| !v.is_null()),
    })
}
