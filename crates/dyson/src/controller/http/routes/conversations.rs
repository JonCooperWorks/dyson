// ===========================================================================
// /api/conversations — list / create / get / delete / cancel.
//
// The list endpoint merges in-memory chats with the disk's mtime-sorted
// list so a chat started over Telegram surfaces in the web sidebar at
// the next list call.  `get` lazily hydrates a `ChatHandle` for chat
// ids that the controller learned about from disk and appends a
// synthetic assistant turn carrying the chat's artefact chips so they
// survive a browser refresh.
// ===========================================================================

use std::sync::Arc;

use hyper::Request;

use crate::message::{ContentBlock, Message, Role};

use super::super::responses::{Resp, bad_request, json_ok, not_found, read_json_capped};
use super::super::state::{ChatHandle, HttpState};
use super::super::stores::ArtefactStore;
use super::super::wire::{BlockDto, ConversationDto, CreateChatBody, MAX_SMALL_BODY, MessageDto};

pub(super) async fn list(state: &HttpState) -> Resp {
    // Prefer the disk's mtime-sorted list when a ChatHistory is
    // configured — Telegram and HTTP share the same on-disk chat dir,
    // so asking disk rather than our in-memory `order` vec means a
    // message sent on Telegram bubbles that chat to the top of the
    // HTTP sidebar at the next list call.  `disk::list()` already
    // sorts newest-first by `transcript.json` mtime.
    let disk_order: Option<Vec<String>> = state
        .history
        .as_ref()
        .and_then(|h| h.list().ok());
    let mut order = match disk_order {
        Some(o) if !o.is_empty() => o,
        _ => state.order.lock().await.clone(),
    };
    // Merge in any in-memory chat ids the disk didn't surface (brand
    // new, transcript not yet flushed) so a just-minted HTTP chat
    // still shows up immediately.
    {
        let mem_order = state.order.lock().await;
        let seen: std::collections::HashSet<&str> =
            order.iter().map(String::as_str).collect();
        let extras: Vec<String> = mem_order
            .iter()
            .filter(|id| !seen.contains(id.as_str()))
            .cloned()
            .collect();
        for id in extras.into_iter().rev() {
            order.insert(0, id);
        }
    }

    // Hydrate handles for chat ids we learned about from disk
    // (typically Telegram chats created while this process was
    // running).  Title is a best-effort read of the first user-text
    // line; a missing/corrupt transcript falls back to the id.
    // Titles cache cuts O(n) history loads per list call to one
    // per chat ever — the cache is invalidated by `turns.rs` when
    // a save changes the first user text.
    {
        let mut chats = state.chats.lock().await;
        let mut titles = match state.titles.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for id in order.iter() {
            if chats.contains_key(id) {
                continue;
            }
            let title = if let Some(t) = titles.get(id) {
                t.clone()
            } else {
                let t = state
                    .history
                    .as_ref()
                    .and_then(|h| h.load(id).ok())
                    .and_then(|msgs| first_user_text(&msgs))
                    .unwrap_or_else(|| id.clone());
                titles.insert(id.clone(), t.clone());
                t
            };
            chats.insert(id.clone(), Arc::new(ChatHandle::new(title)));
        }
    }

    // Build a set of chat ids that own at least one artefact.  Cheap
    // because it's just the in-memory index plus a one-shot scan of
    // each chat's `artefacts/` subdir for chats whose reports have
    // aged out of the FIFO cache.
    let mut with_artefacts: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        for entry in store.items.values() {
            with_artefacts.insert(entry.chat_id.clone());
        }
    }
    if let Some(dir) = state.data_dir.as_ref() {
        for id in order.iter() {
            if with_artefacts.contains(id) {
                continue;
            }
            let sub = ArtefactStore::dir_for_chat(dir, id);
            if std::fs::read_dir(&sub)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|x| x == "json")
                })
            {
                with_artefacts.insert(id.clone());
            }
        }
    }
    let chats = state.chats.lock().await;
    let mut dtos = Vec::with_capacity(order.len());
    for id in order.iter() {
        if let Some(h) = chats.get(id) {
            dtos.push(ConversationDto {
                id: id.clone(),
                title: h.title.clone(),
                live: h.busy.load(std::sync::atomic::Ordering::Relaxed),
                has_artefacts: with_artefacts.contains(id),
                source: source_for_chat_id(id),
            });
        }
    }
    json_ok(&dtos)
}

/// Classify a chat id by its mint convention.  HTTP-minted ids are
/// `c-NNNN` (see `mint_id`); everything else is a Telegram chat id
/// (bare numeric string from `teloxide::types::ChatId`).  Used by the
/// conversation DTO so the sidebar can badge Telegram rows.
pub(crate) fn source_for_chat_id(id: &str) -> &'static str {
    if id.starts_with("c-") {
        "http"
    } else {
        "telegram"
    }
}

pub(super) async fn create(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    let body: CreateChatBody = match read_json_capped(req, MAX_SMALL_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    // Rotate the caller-supplied previous chat first so "+ New
    // Conversation" produces a dated archive the same way /clear does.
    // Best-effort: a missing chat or IO error is logged but doesn't
    // block creation.  The in-memory agent (if any) gets its messages
    // cleared so a future turn on that id doesn't resurrect stale
    // context from the agent cache.
    if let Some(prev) = body.rotate_previous.as_deref() {
        if let Some(prev_handle) = state.chats.lock().await.get(prev).cloned() {
            if let Some(agent) = prev_handle.agent.lock().await.as_mut() {
                agent.clear();
            }
        }
        // The previous chat's first-user-text is gone after rotate —
        // drop any cached title so the next list call rehydrates.
        if let Ok(mut t) = state.titles.lock() {
            t.remove(prev);
        }
        if let Some(h) = state.history.as_ref() {
            if let Err(e) = h.rotate(prev) {
                tracing::warn!(error = %e, chat_id = %prev, "failed to rotate previous chat");
            }
            // Keep the rotated chat visible across restarts by seeding
            // an empty current file — otherwise `list()` skips it and
            // the sidebar loses both the chat and its artefacts.
            if let Err(e) = h.save(prev, &[]) {
                tracing::warn!(error = %e, chat_id = %prev, "failed to seed empty chat after rotate");
            }
        }
    }
    let id = state.mint_id().await;
    let title = body.title.unwrap_or_else(|| "New conversation".to_string());
    let handle = Arc::new(ChatHandle::new(title.clone()));
    state.chats.lock().await.insert(id.clone(), handle);
    // Newest first — push to front so the sidebar shows new chats on top.
    state.order.lock().await.insert(0, id.clone());
    // Persist immediately so every conversation lives on disk 1:1 with
    // the in-memory list.  Without this an empty chat vanishes on
    // restart — the user would see "1 chat" in the sidebar, restart,
    // and the chat would be gone because nothing was ever saved.  The
    // save is best-effort: an IO failure is logged but doesn't fail
    // creation (the in-memory chat still works for this session).
    if let Some(h) = state.history.as_ref() {
        if let Err(e) = h.save(&id, &[]) {
            tracing::warn!(error = %e, chat_id = %id, "failed to persist new chat");
        }
    }
    json_ok(&serde_json::json!({ "id": id, "title": title }))
}

/// Move `id` to the front of the order list.  Called after every turn
/// so the most recently active chat sits on top.  No-op if the id
/// isn't in the list (shouldn't happen, but cheap to guard).
pub(crate) async fn bump_to_front(state: &HttpState, id: &str) {
    let mut order = state.order.lock().await;
    if let Some(pos) = order.iter().position(|x| x == id) {
        if pos != 0 {
            let entry = order.remove(pos);
            order.insert(0, entry);
        }
    }
}

pub(super) async fn get(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    let agent_guard = handle.agent.lock().await;
    let mut messages: Vec<MessageDto> = match agent_guard.as_ref() {
        // Agent already loaded for this chat — its messages are the truth.
        Some(a) => a.messages().iter().map(message_to_dto).collect(),
        // Agent not built yet — load straight from disk so the transcript
        // shows even before the user types in this session.
        None => match state.history.as_ref() {
            Some(h) => match h.load(id) {
                Ok(msgs) => msgs.iter().map(message_to_dto).collect(),
                Err(_) => Vec::new(),
            },
            None => Vec::new(),
        },
    };
    drop(agent_guard);

    // Artefacts are side-channel — they never land in the conversation
    // history, so a fresh page load from disk shows no chips.  Walk the
    // ArtefactStore for this chat and append a synthetic assistant
    // turn with one `Artefact` block per entry so the chat scroll
    // preserves image / report chips across browser refreshes and
    // controller restarts.
    let artefact_blocks: Vec<BlockDto> = {
        let store = match state.artefacts.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        store
            .order
            .iter()
            .filter_map(|aid| store.items.get(aid).map(|e| (aid, e)))
            .filter(|(_, e)| e.chat_id == id)
            .map(|(aid, e)| BlockDto::Artefact {
                id: aid.clone(),
                kind: e.kind,
                title: e.title.clone(),
                url: format!("/#/artefacts/{aid}"),
                bytes: e.content.len(),
                tool_use_id: e.tool_use_id.clone(),
                metadata: e.metadata.clone(),
            })
            .collect()
    };
    if !artefact_blocks.is_empty() {
        messages.push(MessageDto {
            role: "assistant".to_string(),
            blocks: artefact_blocks,
        });
    }

    json_ok(&serde_json::json!({
        "id": id,
        "title": handle.title,
        "messages": messages,
    }))
}

/// Pluck the first user-text block from a message list — used as a chat
/// title hint when hydrating from disk.  Truncated to 60 chars.
pub(crate) fn first_user_text(messages: &[Message]) -> Option<String> {
    for m in messages {
        if matches!(m.role, Role::User) {
            for b in &m.content {
                if let ContentBlock::Text { text } = b {
                    let mut t: String = text.lines().next().unwrap_or("").to_string();
                    if t.chars().count() > 60 {
                        t = t.chars().take(60).collect::<String>() + "…";
                    }
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn message_to_dto(m: &Message) -> MessageDto {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
    .to_string();
    let blocks = m.content.iter().map(block_to_dto).collect();
    MessageDto { role, blocks }
}

fn block_to_dto(b: &ContentBlock) -> BlockDto {
    match b {
        ContentBlock::Text { text } => BlockDto::Text { text: text.clone() },
        ContentBlock::Thinking { thinking } => BlockDto::Thinking {
            thinking: thinking.clone(),
        },
        ContentBlock::ToolUse { id, name, input } => BlockDto::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => BlockDto::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
        ContentBlock::Artefact { id, kind, title } => BlockDto::Artefact {
            id: id.clone(),
            kind: *kind,
            title: title.clone(),
            url: format!("/#/artefacts/{id}"),
            bytes: 0,
            tool_use_id: None,
            metadata: None,
        },
        // User-uploaded image from chat history.  Emit as a data URL
        // so the FileBlock renders inline without a second round-trip.
        // Chat history already shrinks the transcript itself by
        // externalising these to `{chat_dir}/media/<hash>.b64` — we're
        // just the last-hop re-hydration for the browser.
        ContentBlock::Image { data, media_type } => {
            // Rough decoded byte count: base64 is ~4/3 of the raw size.
            let bytes = data.len().saturating_mul(3) / 4;
            BlockDto::File {
                name: format!("image.{}", image_ext_for(media_type)),
                mime: media_type.clone(),
                bytes,
                url: format!("data:{media_type};base64,{data}"),
                inline_image: true,
            }
        }
        // PDFs: render as a download chip.  The extracted text lives
        // in `extracted_text` but isn't useful to surface inline in
        // the transcript — the download link lets the user open the
        // original.
        ContentBlock::Document {
            data,
            extracted_text,
        } => {
            let bytes = data.len().saturating_mul(3) / 4;
            BlockDto::File {
                name: if extracted_text.is_empty() {
                    "document.pdf".to_string()
                } else {
                    // Cheap title: first non-empty line of the extract,
                    // truncated.  Falls back to `document.pdf`.
                    let title = extracted_text
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("document.pdf")
                        .trim()
                        .chars()
                        .take(60)
                        .collect::<String>();
                    if title.is_empty() {
                        "document.pdf".to_string()
                    } else {
                        format!("{title}.pdf")
                    }
                },
                mime: "application/pdf".to_string(),
                bytes,
                url: format!("data:application/pdf;base64,{data}"),
                inline_image: false,
            }
        }
    }
}

/// Best-effort MIME-to-extension mapping for user-uploaded images.
/// Falls back to `png` for unknown types so the browser at least has
/// something to save under when the user clicks the attachment.
fn image_ext_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        _ => "png",
    }
}

pub(super) async fn delete(state: &HttpState, id: &str) -> Resp {
    // Sidebar dismiss.  Empty chats (no in-memory agent messages AND
    // no saved transcript) hard-delete their `{id}.json` — otherwise
    // a freshly-minted chat the user cancels leaves a zero-byte file
    // stranded on disk.  Non-empty chats rotate instead so the
    // transcript survives as a dated archive the user can still grep.
    let handle = match state.chats.lock().await.remove(id) {
        Some(h) => h,
        None => return not_found(),
    };
    state.order.lock().await.retain(|x| x != id);
    // Drop the title cache entry — the chat is gone or rotated.
    if let Ok(mut t) = state.titles.lock() {
        t.remove(id);
    }

    // Cancel any in-flight turn before we drop the handle so the
    // agent doesn't keep streaming into a chat the sidebar forgot.
    if let Some(cancel) = handle.cancel.lock().await.as_ref() {
        cancel.cancel();
    }

    let in_memory_empty = match handle.agent.lock().await.as_ref() {
        Some(a) => a.messages().is_empty(),
        None => true,
    };

    let mut preserved = false;
    if let Some(h) = state.history.as_ref() {
        let disk_empty = h.load(id).map(|m| m.is_empty()).unwrap_or(true);
        if in_memory_empty && disk_empty {
            if let Err(e) = h.remove(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to remove empty chat");
            }
        } else {
            if let Err(e) = h.rotate(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to rotate deleted chat");
            }
            preserved = true;
        }
    }

    json_ok(&serde_json::json!({ "ok": true, "deleted": true, "preserved": preserved }))
}

pub(super) async fn cancel(state: &HttpState, id: &str) -> Resp {
    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };
    if let Some(cancel) = handle.cancel.lock().await.as_ref() {
        cancel.cancel();
    }
    json_ok(&serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_for_chat_id_distinguishes_http_from_telegram() {
        assert_eq!(source_for_chat_id("c-0001"), "http");
        assert_eq!(source_for_chat_id("c-9999"), "http");
        // Telegram chat ids are bare numerics.
        assert_eq!(source_for_chat_id("12345"), "telegram");
        assert_eq!(source_for_chat_id("-100123456"), "telegram");
        // A nonsense id falls in the "telegram" bucket because
        // anything not minted by `mint_id` is treated as foreign.
        assert_eq!(source_for_chat_id("magic"), "telegram");
    }

    #[test]
    fn first_user_text_picks_first_user_message() {
        let msgs = vec![
            Message::user("hello world"),
            Message::assistant(vec![ContentBlock::Text { text: "hi back".into() }]),
        ];
        assert_eq!(first_user_text(&msgs).as_deref(), Some("hello world"));
    }

    #[test]
    fn first_user_text_truncates_long_titles() {
        let long = "a".repeat(200);
        let msgs = vec![Message::user(&long)];
        let title = first_user_text(&msgs).unwrap();
        assert!(title.chars().count() <= 61, "title was {title}");
        assert!(title.ends_with('…'));
    }

    #[test]
    fn first_user_text_skips_assistant_only() {
        let msgs = vec![Message::assistant(vec![ContentBlock::Text {
            text: "no user here".into(),
        }])];
        assert_eq!(first_user_text(&msgs), None);
    }

    #[test]
    fn block_to_dto_emits_data_url_for_images_and_pdf_attachments() {
        // Round-trip via the DTO so the SPA's FileBlock renderer keeps
        // working when a chat hydrates from disk.
        let img = ContentBlock::Image {
            data: "Zm9v".to_string(), // base64("foo")
            media_type: "image/png".to_string(),
        };
        match block_to_dto(&img) {
            BlockDto::File { url, mime, inline_image, .. } => {
                assert!(url.starts_with("data:image/png;base64,Zm9v"));
                assert_eq!(mime, "image/png");
                assert!(inline_image);
            }
            _ => panic!("expected File block"),
        }
        let pdf = ContentBlock::Document {
            data: "JVBERi0xLjQ=".to_string(),
            extracted_text: "Title\nbody".to_string(),
        };
        match block_to_dto(&pdf) {
            BlockDto::File { url, mime, inline_image, name, .. } => {
                assert!(url.starts_with("data:application/pdf;base64,"));
                assert_eq!(mime, "application/pdf");
                assert!(!inline_image);
                assert!(name.starts_with("Title"));
            }
            _ => panic!("expected File block"),
        }
    }

    #[test]
    fn image_ext_for_falls_back_to_png() {
        assert_eq!(image_ext_for("image/jpeg"), "jpg");
        assert_eq!(image_ext_for("image/jpg"), "jpg");
        assert_eq!(image_ext_for("image/png"), "png");
        assert_eq!(image_ext_for("image/heic"), "heic");
        assert_eq!(image_ext_for("image/webp"), "webp");
        assert_eq!(image_ext_for("image/gif"), "gif");
        assert_eq!(image_ext_for("image/avif"), "png", "unknown falls back");
        assert_eq!(image_ext_for(""), "png");
    }
}
