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
        // /clear also drops anything queued — the user reset the chat
        // and would not expect prompts they typed during a previous run
        // to resurrect.  Cancel does NOT drain (queued messages there
        // are independent intentions); only /clear wipes them.
        handle.clear_queued().await;
        handle.emit(SseEvent::Done);
        // /clear ends any in-flight stream — wipe the replay ring so a
        // subsequent send doesn't see this turn's events.
        handle.reset_replay();
        return json_ok(&serde_json::json!({ "ok": true, "cleared": true }));
    }

    let prev_busy = handle
        .busy
        .swap(true, std::sync::atomic::Ordering::SeqCst);

    // Quiesce gate.  Order of operations matters: we ALWAYS swap busy
    // before reading `quiesced` so the SeqCst pair on this side
    // interlocks with the SeqCst pair in `routes::admin::post_quiesce`
    // (store quiesced → load all `busy`).  Either:
    //   - we passed busy.swap before the quiescer's store → the
    //     quiescer's busy scan sees us → it 409s, we keep running;
    //   - we passed busy.swap after the quiescer's store → we see
    //     quiesced=true here → we undo busy and 503.
    // The race window where both succeed is closed by SeqCst.
    if state.quiesced.load(std::sync::atomic::Ordering::SeqCst) {
        if !prev_busy {
            handle
                .busy
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Content-Type", "application/json")
            .header("Retry-After", "30")
            .body(boxed(Bytes::from_static(
                br#"{"error":"instance is quiescing for upgrade"}"#,
            )))
            .unwrap();
    }

    if prev_busy {
        // Already running a turn for this chat — try to enqueue the
        // new POST instead of rejecting it.  When the in-flight turn
        // ends, the spawned task drains the queue and runs one more
        // coalesced agent.run(); if more arrive during that run, they
        // queue again and the loop repeats.  Persisted to disk so a
        // restart mid-turn doesn't drop messages the user typed.
        let queued = super::super::state::QueuedTurn {
            prompt: body.prompt,
            attachments: body
                .attachments
                .into_iter()
                .map(|a| super::super::state::QueuedAttachment {
                    mime_type: a.mime_type,
                    name: a.name,
                    data_base64: a.data_base64,
                })
                .collect(),
        };
        return match handle.enqueue_turn(queued).await {
            super::super::state::EnqueueResult::Queued { position } => json_ok(
                &serde_json::json!({"ok": true, "queued": true, "position": position}),
            ),
            super::super::state::EnqueueResult::Full => Response::builder()
                .status(StatusCode::CONFLICT)
                .header("Content-Type", "application/json")
                .body(boxed(Bytes::from_static(
                    br#"{"error":"chat queue is full"}"#,
                )))
                .unwrap(),
        };
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
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: spawned");
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
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: acquiring agent lock");
        let mut guard = chat_handle.agent.lock().await;
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: agent lock acquired");
        // Prefer the runtime-selected provider's client when one
        // is set — falls back to the registry default otherwise
        // (unknown provider name or no override set).
        let client = match override_provider_name.as_deref() {
            Some(p) => registry.get(p).unwrap_or_else(|_| registry.get_default()),
            None => registry.get_default(),
        };
        if guard.is_none() {
            tracing::info!(chat_id = %chat_id, "TURN_WORKER: agent is None — calling build_agent");
            match build_agent(&settings, None, AgentMode::Private, client.clone(), &registry, None).await {
                Ok(mut a) => {
                    tracing::info!(chat_id = %chat_id, "TURN_WORKER: build_agent OK");
                    if let Some(h) = history.as_ref() {
                        match h.load(&chat_id) {
                            Ok(msgs) if !msgs.is_empty() => a.set_messages(msgs),
                            _ => {}
                        }
                    }
                    *guard = Some(a);
                    // Watch BOTH dyson.json and the workspace.  dyson.json
                    // catches swarm's runtime `/api/admin/configure`
                    // patches (proxy_token / proxy_base / models); the
                    // workspace catches dream-driven skill writes and
                    // manual edits to MEMORY.md / SOUL.md.  Without
                    // dyson.json in the per-chat watch set, the cached
                    // agent kept its first-turn `warmup-placeholder`
                    // client for the lifetime of the chat — every
                    // subsequent turn 401'd against the warmup default.
                    // Mirrors what `check_and_reload_agent` does for the
                    // terminal controller.
                    let workspace_path = crate::workspace::FilesystemWorkspace::resolve_path(
                        Some(settings.workspace.connection_string.expose()),
                    );
                    let config_path = crate::controller::resolve_config_path_for_runtime(None);
                    *chat_handle.reloader.lock().await = Some(
                        crate::config::hot_reload::HotReloader::new(
                            config_path.as_deref(),
                            workspace_path.as_deref(),
                        ),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, chat_id = %chat_id, "agent build failed");
                    chat_handle.emit(SseEvent::LlmError {
                        message: format!("agent build failed: {}", e.sanitized_message()),
                    });
                    chat_handle.emit(SseEvent::Done);
                    // Wipe the per-turn replay ring before releasing
                    // busy so a fresh EventSource opening for the next
                    // attempt doesn't replay this aborted turn's
                    // events into the new agent placeholder.
                    chat_handle.reset_replay();
                    chat_handle
                        .busy
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
        } else {
            // Hot-reload: if dyson.json (swarm's `/api/admin/configure`
            // patch — proxy_token / proxy_base / models) or workspace
            // files (skills written by SelfImprovementDream, manual
            // edits to MEMORY.md / SOUL.md) changed since the last turn
            // for this chat, rebuild the cached agent so the new config
            // takes effect.  Without watching dyson.json the cached
            // agent pins its first-turn client (the registry's
            // `warmup-placeholder` stub for a freshly-restored sandbox)
            // for the lifetime of the chat.  Mirrors what
            // `check_and_reload_agent` does for the terminal controller.
            let changed = match chat_handle.reloader.lock().await.as_mut() {
                Some(r) => match r.check().await {
                    Ok((c, _)) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, chat_id = %chat_id, "config/workspace reload check failed");
                        false
                    }
                },
                None => false,
            };
            if changed {
                let messages = guard
                    .as_ref()
                    .map(|a| a.messages().to_vec())
                    .unwrap_or_default();
                match build_agent(&settings, None, AgentMode::Private, client.clone(), &registry, None).await {
                    Ok(mut a) => {
                        a.set_messages(messages);
                        *guard = Some(a);
                        tracing::info!(chat_id = %chat_id, "agent rebuilt: dyson.json or workspace changed");
                    }
                    Err(e) => {
                        // Keep the previous agent — a partial failure
                        // shouldn't take the chat down.  The next turn
                        // will retry the rebuild if the watcher still
                        // sees a delta.
                        tracing::warn!(error = %e, chat_id = %chat_id, "agent rebuild after config/workspace change failed; reusing previous agent");
                    }
                }
            }
        }
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: agent ready, wiring run");
        let agent = guard.as_mut().expect("agent built or kept above");
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
        // Wire the subagent UI events bus so nested tool calls inside
        // any subagent for this turn surface live in the right rail.
        // Cloning the chat's broadcast Sender is cheap (it's Arc-y);
        // the bus does not push into the replay ring (live-only), so
        // a reconnect mid-subagent shows the panel empty until the
        // next nested event arrives.  See `SubagentEventBus`.
        agent.set_subagent_events(super::super::SubagentEventBus::new(
            chat_handle.events.clone(),
        ));
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
        //
        // After the initial run we drain any POSTs that arrived while
        // busy and run them as one coalesced sub-turn; if more arrive
        // during that drain run they queue again and the loop repeats.
        // Cancellation aborts the current sub-turn and exits the loop —
        // the queue stays persisted so the next POST or restart picks
        // it up.
        let mut next_prompt = prompt;
        let mut next_attachments = attachments;
        loop {
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
                    tracing::info!(chat_id = %chat_id, prompt_len = next_prompt.len(), atts = next_attachments.len(), "TURN_WORKER: calling agent.run");
                    let r = if next_attachments.is_empty() {
                        agent.run(&next_prompt, &mut output).await
                    } else {
                        let atts = std::mem::take(&mut next_attachments);
                        agent.run_with_attachments(&next_prompt, atts, &mut output).await
                    };
                    tracing::info!(chat_id = %chat_id, ok = r.is_ok(), "TURN_WORKER: agent.run returned");
                    r
                } => r,
            };
            match result {
                Ok(_) => {}
                Err(e) => {
                    // Full Display goes to the operator log; the SSE wire
                    // gets the sanitised form so cross-tenant deployments
                    // don't leak filesystem paths or upstream URLs.
                    tracing::warn!(error = %e, chat_id = %chat_id, "turn failed");
                    chat_handle.emit(SseEvent::LlmError {
                        message: e.sanitized_message(),
                    });
                }
            }

            // Persist the conversation to disk after every (sub-)turn.
            // Canonical save point — controllers/telegram does the same.
            if let Some(h) = history.as_ref()
                && let Err(e) = h.save(&chat_id, agent.messages()) {
                    tracing::warn!(error = %e, chat_id = %chat_id, "failed to save chat history");
                }

            chat_handle.emit(SseEvent::Done);
            // Wipe the per-turn replay ring before either looping or
            // releasing busy so a fresh EventSource opening for the
            // *next* turn doesn't replay this turn's events into the
            // new agent placeholder.  See
            // sse_fresh_connect_after_turn_end_does_not_replay_stale_events.
            chat_handle.reset_replay();

            // Cancellation: stop draining; leave the queue alone for
            // the next POST or restart to pick up.
            if cancel_for_select.is_cancelled() {
                break;
            }

            // Drain anything that queued up during the run we just
            // finished; coalesce into one prompt and loop.
            let drained = chat_handle.drain_queued_turns().await;
            if drained.is_empty() {
                break;
            }
            let (combined_prompt, combined_attachments) = coalesce_queued(drained);
            next_prompt = combined_prompt;
            next_attachments = combined_attachments;
        }

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

/// Combine N queued turns into a single prompt + attachment set so the
/// agent sees one coherent user message instead of having to deduce
/// they came from separate POSTs.  Single-turn drains pass through
/// unchanged; multi-turn drains get a numbered preamble so the agent
/// answers them as discrete asks.
pub(crate) fn coalesce_queued(
    turns: Vec<super::super::state::QueuedTurn>,
) -> (String, Vec<crate::media::Attachment>) {
    let mut all_attachments = Vec::new();
    for t in &turns {
        for a in &t.attachments {
            // Validated at enqueue, but defend against on-disk
            // corruption — a single bad attachment must not abort the
            // whole drain.
            match base64::engine::general_purpose::STANDARD.decode(a.data_base64.as_bytes()) {
                Ok(bytes) => all_attachments.push(crate::media::Attachment {
                    data: bytes,
                    mime_type: a.mime_type.clone(),
                    file_name: a.name.clone(),
                }),
                Err(e) => tracing::warn!(error = %e, name = ?a.name, "queued attachment had malformed base64; skipping"),
            }
        }
    }
    let prompt = if turns.len() == 1 {
        turns.into_iter().next().expect("len==1 checked").prompt
    } else {
        use std::fmt::Write as _;
        let mut s = format!(
            "I sent {} more messages while you were working:\n\n",
            turns.len()
        );
        for (i, t) in turns.iter().enumerate() {
            let _ = write!(s, "{}. {}\n\n", i + 1, t.prompt);
        }
        s.trim_end().to_string()
    };
    (prompt, all_attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::state::{QueuedAttachment, QueuedTurn};

    #[test]
    fn coalesce_single_turn_passes_through() {
        let (prompt, atts) = coalesce_queued(vec![QueuedTurn {
            prompt: "hello".into(),
            attachments: vec![],
        }]);
        assert_eq!(prompt, "hello");
        assert!(atts.is_empty());
    }

    #[test]
    fn coalesce_multi_turn_numbers_with_preamble() {
        let (prompt, _) = coalesce_queued(vec![
            QueuedTurn { prompt: "first".into(), attachments: vec![] },
            QueuedTurn { prompt: "second".into(), attachments: vec![] },
            QueuedTurn { prompt: "third".into(), attachments: vec![] },
        ]);
        assert!(prompt.starts_with("I sent 3 more messages while you were working:"));
        assert!(prompt.contains("1. first"));
        assert!(prompt.contains("2. second"));
        assert!(prompt.contains("3. third"));
    }

    #[test]
    fn coalesce_merges_attachments_in_order() {
        use base64::Engine;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let (_, atts) = coalesce_queued(vec![
            QueuedTurn {
                prompt: "a".into(),
                attachments: vec![QueuedAttachment {
                    mime_type: "image/png".into(),
                    name: Some("one.png".into()),
                    data_base64: b64(b"\x89PNG one"),
                }],
            },
            QueuedTurn {
                prompt: "b".into(),
                attachments: vec![QueuedAttachment {
                    mime_type: "image/png".into(),
                    name: Some("two.png".into()),
                    data_base64: b64(b"\x89PNG two"),
                }],
            },
        ]);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].file_name.as_deref(), Some("one.png"));
        assert_eq!(atts[1].file_name.as_deref(), Some("two.png"));
        assert_eq!(atts[0].data, b"\x89PNG one");
        assert_eq!(atts[1].data, b"\x89PNG two");
    }

    #[test]
    fn coalesce_skips_malformed_base64_attachment() {
        let (_, atts) = coalesce_queued(vec![QueuedTurn {
            prompt: "x".into(),
            attachments: vec![QueuedAttachment {
                mime_type: "image/png".into(),
                name: Some("bad.png".into()),
                data_base64: "not!!!base64".into(),
            }],
        }]);
        assert!(atts.is_empty(), "malformed attachment must be skipped, not panic");
    }

    /// Regression for the chat-stuck-on-warmup-placeholder bug.
    ///
    /// The per-chat HotReloader watches dyson.json so swarm's runtime
    /// `/api/admin/configure` patches (proxy_token / proxy_base /
    /// models) trigger a per-chat agent rebuild on the next turn.
    /// Before the fix this reloader was constructed with `None` for
    /// the config path, so the cached agent kept its first-turn
    /// `warmup-placeholder` client for the lifetime of the chat —
    /// every turn 401'd against `https://api.openai.com` with the
    /// placeholder bearer.
    ///
    /// This test stands in for the live setup: a reloader anchored on
    /// a dyson.json file must report the file-changed signal that
    /// drives the rebuild branch in the spawned task above.
    #[tokio::test]
    async fn config_changes_are_picked_up_by_per_chat_reloader() {
        use std::time::Duration;
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = dir.path().join("dyson.json");
        std::fs::write(&cfg, r#"{"agent":{}}"#).expect("seed config");

        let mut reloader = crate::config::hot_reload::HotReloader::new(
            Some(&cfg),
            None,
        );

        let (changed, _) = reloader.check().await.expect("check");
        assert!(!changed, "fresh reloader must not falsely report change");

        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(
            &cfg,
            r#"{"agent":{},"providers":{"openrouter":{"type":"openai","api_key":"dy-real","base_url":"https://swarm.example/llm/openrouter"}}}"#,
        )
        .expect("rewrite config");

        let (changed, _) = reloader.check().await.expect("check after change");
        assert!(
            changed,
            "the per-chat reloader must observe dyson.json changes — \
             without this the cached agent keeps its warmup-placeholder client \
             for the lifetime of the chat"
        );
    }
}
