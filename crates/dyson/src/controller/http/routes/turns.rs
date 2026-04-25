// ===========================================================================
// /api/conversations/:id/turn — kick off an agent run.
//
// Returns 202 immediately; the agent's stream lands on the chat's
// `/events` SSE channel.  The /clear slash command is intercepted
// before the busy latch so it can rotate the transcript without
// running a turn.  POST /cancel feeds a CancellationToken stitched
// into the outer `tokio::select!` so cancellation aborts the run at
// the next await point — the persist hook has already checkpointed
// everything the agent committed.
// ===========================================================================

use std::sync::Arc;

use base64::Engine;
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use tokio_util::sync::CancellationToken;

use super::super::output::SseOutput;
use super::super::responses::{Resp, bad_request, boxed, json_ok, not_found, read_json_capped};
use super::super::state::HttpState;
use super::super::wire::{MAX_TURN_BODY, SseEvent, TurnBody};
use super::super::{AgentMode, build_agent};
use super::conversations::bump_to_front;

pub(super) async fn post(
    req: Request<hyper::body::Incoming>,
    state: Arc<HttpState>,
    id: &str,
) -> Resp {
    // Reject oversized bodies before buffering — a 100MB upload would
    // pin a request worker and waste memory.
    if let Some(cl) = req.headers().get("content-length")
        && let Some(len) = cl.to_str().ok().and_then(|s| s.parse::<usize>().ok())
        && len > MAX_TURN_BODY
    {
        return bad_request(&format!("request body too large ({len} bytes; max {MAX_TURN_BODY})"));
    }
    let body: TurnBody = match read_json_capped(req, MAX_TURN_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };

    // Decode attachments up front so a malformed base64 fails the
    // request before we kick off the agent (clean rejection > orphan
    // SSE done event).
    let mut decoded: Vec<crate::media::Attachment> = Vec::with_capacity(body.attachments.len());
    for a in &body.attachments {
        match base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes()) {
            Ok(bytes) => decoded.push(crate::media::Attachment {
                data: bytes,
                mime_type: a.mime_type.clone(),
                file_name: a.name.clone(),
            }),
            Err(e) => return bad_request(&format!("attachment '{}' base64 decode failed: {e}",
                a.name.as_deref().unwrap_or("<unnamed>"))),
        }
    }

    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };

    // Intercept `/clear` before the busy latch + spawn path.  Without
    // this, the slash command listed in data.js would land at the LLM
    // as a plain prompt and nothing on disk would rotate.  Telegram's
    // `handle_per_chat_command` does the same thing via
    // `execute_agent_command` → `chat_store.rotate`.  Other slash
    // commands (`/compact`, `/model`) require an LLM call or have
    // dedicated endpoints, so they continue to fall through.
    if body.prompt.trim() == "/clear" && decoded.is_empty() {
        if let Some(agent) = handle.agent.lock().await.as_mut() {
            agent.clear();
        }
        // Title cache is keyed by first-user-text — a /clear wipes that,
        // so drop the cached entry to force the next list call to
        // rehydrate from the (now empty) transcript.
        if let Ok(mut t) = state.titles.lock() {
            t.remove(id);
        }
        if let Some(h) = state.history.as_ref() {
            if let Err(e) = h.rotate(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to rotate chat history");
            }
            // Re-create the current file as an empty transcript so the
            // chat stays visible across restarts.  Without this,
            // DiskChatHistory::list() skips it (no current file, only
            // archives) and the sidebar loses the chat — along with the
            // artefacts filtered by its id.
            if let Err(e) = h.save(id, &[]) {
                tracing::warn!(error = %e, chat_id = %id, "failed to seed empty chat after rotate");
            }
        }
        handle.emit(SseEvent::Done);
        return json_ok(&serde_json::json!({ "ok": true, "cleared": true }));
    }

    if handle
        .busy
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .header("Content-Type", "application/json")
            .body(boxed(Bytes::from_static(
                br#"{"error":"chat is busy"}"#,
            )))
            .unwrap();
    }

    // Set up cancellation.
    let cancel = CancellationToken::new();
    *handle.cancel.lock().await = Some(cancel.clone());

    // Apply the runtime model override before handing settings to
    // `build_agent` so a brand-new chat picks up the operator's last
    // model choice instead of the startup default.  `runtime_model`
    // is set by `post_model`; when unset this is a no-op clone.
    let mut settings = state.settings_snapshot();
    let override_pm: Option<(String, String)> = match state.runtime_model.lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    let override_provider_name = if let Some((prov, model)) = override_pm {
        if let Some(pc) = settings.providers.get(&prov) {
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = model;
        }
        Some(prov)
    } else {
        None
    };
    let registry = Arc::clone(&state.registry);
    let history = state.history.clone();
    let prompt = body.prompt;
    let attachments = decoded;
    let chat_handle = Arc::clone(&handle);
    let chat_id = id.to_string();
    let state_for_task = Arc::clone(&state);
    let files = Arc::clone(&state.files);
    let file_id = Arc::clone(&state.file_id);
    let artefacts = Arc::clone(&state.artefacts);
    let artefact_id = Arc::clone(&state.artefact_id);
    let data_dir = state.data_dir.clone();

    tokio::spawn(async move {
        let mut output = SseOutput {
            chat_id: chat_id.clone(),
            tx: chat_handle.events.clone(),
            replay: Arc::clone(&chat_handle.replay),
            files,
            next_file_id: file_id,
            artefacts,
            next_artefact_id: artefact_id,
            data_dir,
            current_tool_use_id: None,
        };

        // Lazily build the agent on first use.  If a transcript exists
        // on disk for this chat_id, replay it into the agent so context
        // carries across sessions.
        let mut guard = chat_handle.agent.lock().await;
        if guard.is_none() {
            // Prefer the runtime-selected provider's client when one
            // is set — falls back to the registry default otherwise
            // (unknown provider name or no override set).
            let client = match override_provider_name.as_deref() {
                Some(p) => registry.get(p).unwrap_or_else(|_| registry.get_default()),
                None => registry.get_default(),
            };
            match build_agent(&settings, None, AgentMode::Private, client, &registry, None).await {
                Ok(mut a) => {
                    if let Some(h) = history.as_ref() {
                        match h.load(&chat_id) {
                            Ok(msgs) if !msgs.is_empty() => a.set_messages(msgs),
                            _ => {}
                        }
                    }
                    *guard = Some(a);
                }
                Err(e) => {
                    chat_handle.emit(SseEvent::LlmError {
                        message: format!("agent build failed: {e}"),
                    });
                    chat_handle.emit(SseEvent::Done);
                    chat_handle
                        .busy
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
        }
        let agent = guard.as_mut().expect("agent built above");
        // The agent polls `cancellation.is_cancelled()` at iteration
        // boundaries, which is fine for /stop between tool calls but
        // useless during a long LLM stream or a multi-second tool.
        // Keep a separate clone of the token so the outer `select!`
        // below can tear the whole run down when the user clicks
        // cancel — the agent future drops at its next await point,
        // which cancels in-flight HTTP requests, tool processes, and
        // streaming reads cooperatively.
        let cancel_for_select = cancel.clone();
        agent.set_cancellation_token(cancel);
        // Wire the chat-scoped activity handle so the Activity tab
        // shows running subagents for this chat.  Rebound on every
        // turn (cheap Arc clone) because the agent is cached and
        // re-used, but handle binding carries chat_id which we only
        // know at this dispatch site.
        agent.set_activity_handle(state.activity.handle_for(&chat_id));
        // Checkpoint-save the transcript to disk after every message
        // push.  Without this, a process kill during a long subagent
        // run (e.g. security_engineer streams for minutes) loses the
        // whole conversation — the end-of-turn save below is
        // unreachable if the tokio task is aborted mid-run.
        if let Some(h) = history.as_ref() {
            let h = Arc::clone(h);
            let chat_id_for_hook = chat_id.clone();
            agent.set_persist_hook(std::sync::Arc::new(move |messages| {
                if let Err(e) = h.save(&chat_id_for_hook, messages) {
                    tracing::warn!(error = %e, chat_id = %chat_id_for_hook, "persist hook failed to save chat history");
                }
            }));
        }

        // Branch on attachments: with attachments, dispatch through
        // run_with_attachments so images/audio/PDF are resolved into
        // multimodal ContentBlocks (same path Telegram takes).
        //
        // Wrap in `tokio::select!` so POST /cancel aborts the run at
        // the next await point instead of waiting for the current LLM
        // stream / tool call to finish on its own.  The persist hook
        // installed above has already checkpointed every message the
        // agent committed to its conversation, so dropping the future
        // mid-run is safe: the state that survives is exactly what
        // the agent had decided on.
        let result = tokio::select! {
            biased;
            _ = cancel_for_select.cancelled() => {
                tracing::info!(chat_id = %chat_id, "turn aborted by cancel request");
                chat_handle.emit(SseEvent::LlmError {
                    message: "cancelled".to_string(),
                });
                Ok(String::new())
            }
            r = async {
                if attachments.is_empty() {
                    agent.run(&prompt, &mut output).await
                } else {
                    agent.run_with_attachments(&prompt, attachments, &mut output).await
                }
            } => r,
        };
        match result {
            Ok(_) => {}
            Err(e) => {
                chat_handle.emit(SseEvent::LlmError {
                    message: e.to_string(),
                });
            }
        }

        // Persist the conversation to disk after every turn.  This is the
        // canonical save point — controllers/telegram does the same.
        if let Some(h) = history.as_ref() {
            if let Err(e) = h.save(&chat_id, agent.messages()) {
                tracing::warn!(error = %e, chat_id = %chat_id, "failed to save chat history");
            }
        }

        chat_handle.emit(SseEvent::Done);
        chat_handle
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Bump this chat to the top of the sidebar list — most-recent
        // activity wins.
        bump_to_front(&state_for_task, &chat_id).await;
    });

    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"ok":true}"#)))
        .unwrap()
}
