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
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::agent::rate_limiter::{Priority, RateLimitedHandle};
use crate::config::Settings;
use crate::controller::Output;
use crate::llm::stream::StreamEvent;
use crate::llm::{CompletionConfig, LlmClient};
use crate::message::{ContentBlock, Message};

use super::super::output::SseOutput;
use super::super::responses::{
    Resp, bad_request, boxed, internal_error, json_ok, not_found, read_json_capped,
    service_unavailable,
};
use super::super::state::{ChatHandle, HttpState, RuntimeModelSelection};
use super::super::wire::{MAX_TURN_BODY, SseEvent, TurnBody};
use super::super::{AgentMode, ClientRegistry, build_agent};
use super::conversations::bump_to_front;

const PLACEHOLDER_TITLE: &str = "New conversation";
const WARMUP_PLACEHOLDER: &str = "warmup-placeholder";

#[derive(Clone)]
struct ReadyTurnConfig {
    settings: Settings,
    provider_name: Option<String>,
}

impl ReadyTurnConfig {
    fn resolve_with_selection(
        state: &HttpState,
        selection: Option<&RuntimeModelSelection>,
    ) -> Result<Self, String> {
        let mut settings = state.settings_snapshot();
        let runtime;
        let selected = if let Some(selection) = selection {
            Some(selection.clone())
        } else {
            runtime = match state.runtime_model.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            runtime
        };
        let provider_name = if let Some(selection) = selected {
            selection.apply_to_settings(&mut settings)?;
            Some(selection.provider().to_string())
        } else {
            crate::controller::active_provider_name(&settings)
        };

        reject_unready_agent_config(&settings, provider_name.as_deref())?;
        Ok(Self {
            settings,
            provider_name,
        })
    }

    fn client(
        &self,
        registry: &ClientRegistry,
    ) -> crate::Result<RateLimitedHandle<Box<dyn LlmClient>>> {
        match self.provider_name.as_deref() {
            Some(provider) => registry.get(provider),
            None => Ok(registry.get_default()),
        }
    }
}

fn reject_unready_agent_config(
    settings: &Settings,
    provider_name: Option<&str>,
) -> Result<(), String> {
    let model = settings.agent.model.trim();
    if model.is_empty() {
        return Err("agent is not configured yet: no model is selected".to_string());
    }
    if model == WARMUP_PLACEHOLDER {
        return Err("agent is not configured yet: swarm warmup model is still active".to_string());
    }
    if provider_name.is_none() && !settings.providers.is_empty() {
        return Err("agent is not configured yet: no active provider is selected".to_string());
    }

    if let Some(provider_name) = provider_name {
        let pc = settings
            .providers
            .get(provider_name)
            .ok_or_else(|| format!("unknown provider '{provider_name}'"))?;
        if pc.api_key.expose() == WARMUP_PLACEHOLDER {
            return Err(format!(
                "agent is not configured yet: provider '{provider_name}' still has the swarm warmup API key"
            ));
        }
    }

    Ok(())
}

fn turn_model_selection(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<Option<RuntimeModelSelection>, String> {
    match (provider, model) {
        (Some(provider), Some(model)) => RuntimeModelSelection::new(provider, model).map(Some),
        (None, None) => Ok(None),
        _ => Err("provider and model must be supplied together".to_string()),
    }
}

fn queued_model_selection(
    turns: &[super::super::state::QueuedTurn],
) -> Option<RuntimeModelSelection> {
    turns.iter().rev().find_map(|t| {
        turn_model_selection(t.provider.as_deref(), t.model.as_deref())
            .ok()
            .flatten()
    })
}

fn apply_model_selection_to_agent(
    agent: &mut crate::agent::Agent,
    state: &HttpState,
    registry: &ClientRegistry,
    selection: &RuntimeModelSelection,
) -> Result<(), String> {
    let snapshot = state.settings_snapshot();
    let provider_cfg = snapshot
        .providers
        .get(selection.provider())
        .ok_or_else(|| format!("unknown provider '{}'", selection.provider()))?;
    let client = registry.get(selection.provider()).map_err(|e| {
        format!(
            "provider '{}' is not ready: {}",
            selection.provider(),
            e.sanitized_message()
        )
    })?;
    agent.swap_client(client, selection.model(), &provider_cfg.provider_type);
    Ok(())
}

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
        return bad_request(&format!(
            "request body too large ({len} bytes; max {MAX_TURN_BODY})"
        ));
    }
    let body: TurnBody = match read_json_capped(req, MAX_TURN_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let requested_model =
        match turn_model_selection(body.provider.as_deref(), body.model.as_deref()) {
            Ok(selection) => selection,
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
            Err(e) => {
                return bad_request(&format!(
                    "attachment '{}' base64 decode failed: {e}",
                    a.name.as_deref().unwrap_or("<unnamed>")
                ));
            }
        }
    }

    let handle = match state.chats.lock().await.get(id).cloned() {
        Some(h) => h,
        None => return not_found(),
    };

    if state.is_quiesced() {
        return service_unavailable("instance is quiesced for maintenance");
    }

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
        handle.set_title(PLACEHOLDER_TITLE.to_string());
        if let Some(h) = state.history.as_ref() {
            if let Err(e) = h.rotate(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to rotate chat history");
            }
            if let Err(e) = h.remove_title(id) {
                tracing::warn!(error = %e, chat_id = %id, "failed to remove chat title");
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
        handle.emit(SseEvent::Title {
            title: PLACEHOLDER_TITLE.to_string(),
        });
        handle.emit(SseEvent::Done);
        // /clear ends any in-flight stream — wipe the replay ring so a
        // subsequent send doesn't see this turn's events.
        handle.reset_replay();
        return json_ok(&serde_json::json!({ "ok": true, "cleared": true }));
    }

    if handle.busy.swap(true, std::sync::atomic::Ordering::SeqCst) {
        // Already running a turn for this chat — try to enqueue the
        // new POST instead of rejecting it.  When the in-flight turn
        // ends, the spawned task drains the queue and runs one more
        // coalesced agent.run(); if more arrive during that run, they
        // queue again and the loop repeats.  Persisted to disk so a
        // restart mid-turn doesn't drop messages the user typed.
        let queued = super::super::state::QueuedTurn {
            prompt: body.prompt,
            provider: body.provider,
            model: body.model,
            queue_mode: body.queue_mode,
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
            super::super::state::EnqueueResult::Queued { position } => {
                json_ok(&serde_json::json!({"ok": true, "queued": true, "position": position}))
            }
            super::super::state::EnqueueResult::Full => Response::builder()
                .status(StatusCode::CONFLICT)
                .header("Content-Type", "application/json")
                .body(boxed(Bytes::from_static(
                    br#"{"error":"chat queue is full"}"#,
                )))
                .unwrap(),
        };
    }
    if state.is_quiesced() {
        handle
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);
        return service_unavailable("instance is quiesced for maintenance");
    }

    let prompt = body.prompt;
    let attachments = decoded;

    let direct_settings = state.settings_snapshot();
    if crate::controller::slash::should_handle_without_llm(&direct_settings, &prompt) {
        spawn_direct_slash_turn(
            Arc::clone(&state),
            Arc::clone(&handle),
            id.to_string(),
            direct_settings,
            prompt,
            !attachments.is_empty(),
        );
        return accepted_turn_response();
    }

    let registry = Arc::clone(&state.registry);
    let turn_config =
        match ReadyTurnConfig::resolve_with_selection(&state, requested_model.as_ref()) {
            Ok(config) => config,
            Err(e) => {
                handle
                    .busy
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return service_unavailable(&e);
            }
        };
    let client = match turn_config.client(&registry) {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(error = %e, chat_id = %id, "turn provider is not ready");
            handle
                .busy
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return service_unavailable(&format!(
                "agent provider is not ready: {}",
                e.sanitized_message()
            ));
        }
    };
    let settings = turn_config.settings;

    let history = state.history.clone();

    let checkpoint_message = accepted_turn_checkpoint_message(&prompt, &attachments);
    let checkpoint_saved = match checkpoint_accepted_turn(
        &handle,
        history.as_ref(),
        id,
        &checkpoint_message,
    )
    .await
    {
        Ok(saved) => saved,
        Err(e) => {
            tracing::warn!(error = %e, chat_id = %id, "failed to checkpoint accepted user turn");
            handle
                .busy
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return internal_error("failed to persist accepted turn");
        }
    };

    // Set up cancellation only after configuration and the accepted-turn
    // checkpoint are ready.  A warmup or persistence failure should return
    // before installing per-turn state.
    let cancel = CancellationToken::new();
    *handle.cancel.lock().await = Some(cancel.clone());

    let should_title = title_needs_generation(&handle.title()) && !prompt.trim().is_empty();
    let chat_handle = Arc::clone(&handle);
    let chat_id = id.to_string();
    let state_for_task = Arc::clone(&state);
    let files = Arc::clone(&state.files);
    let file_id = Arc::clone(&state.file_id);
    let artefacts = Arc::clone(&state.artefacts);
    let artefact_id = Arc::clone(&state.artefact_id);
    let data_dir = state.data_dir.clone();
    let artefact_reader = super::super::artefact_access::HttpArtefactReader::new(
        Arc::clone(&artefacts),
        data_dir.clone(),
    );
    let ingest = Arc::clone(&state.ingest);

    if should_title {
        let title_client = client.clone().with_priority(Priority::Background);
        spawn_title_generation(
            Arc::clone(&state),
            Arc::clone(&handle),
            id.to_string(),
            prompt.clone(),
            settings.agent.model.clone(),
            title_client,
        );
    }

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
            pending: Some(Arc::clone(&chat_handle)),
            current_tool_use_id: None,
            ingest,
        };

        // Lazily build the agent on first use.  If a transcript exists
        // on disk for this chat_id, replay it into the agent so context
        // carries across sessions.
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: acquiring agent lock");
        let mut guard = chat_handle.agent.lock().await;
        tracing::info!(chat_id = %chat_id, "TURN_WORKER: agent lock acquired");
        let mut loaded_checkpoint_tail = false;
        if guard.is_none() {
            tracing::info!(chat_id = %chat_id, "TURN_WORKER: agent is None — calling build_agent");
            match build_agent(
                &settings,
                None,
                AgentMode::Private,
                client.clone(),
                &registry,
                None,
            )
            .await
            {
                Ok(mut a) => {
                    tracing::info!(chat_id = %chat_id, "TURN_WORKER: build_agent OK");
                    if let Some(h) = history.as_ref() {
                        match h.load(&chat_id) {
                            Ok(msgs) if !msgs.is_empty() => {
                                loaded_checkpoint_tail = checkpoint_saved
                                    && msgs
                                        .last()
                                        .is_some_and(|m| same_message(m, &checkpoint_message));
                                a.set_messages(msgs);
                            }
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
                    let workspace_path = crate::workspace::FilesystemWorkspace::resolve_path(Some(
                        settings.workspace.connection_string.expose(),
                    ));
                    let config_path = crate::controller::resolve_config_path_for_runtime(None);
                    *chat_handle.reloader.lock().await =
                        Some(crate::config::hot_reload::HotReloader::new(
                            config_path.as_deref(),
                            workspace_path.as_deref(),
                        ));
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
                match build_agent(
                    &settings,
                    None,
                    AgentMode::Private,
                    client.clone(),
                    &registry,
                    None,
                )
                .await
                {
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
        agent.set_artefact_reader(Arc::new(artefact_reader), chat_id.clone());
        if loaded_checkpoint_tail {
            drop_checkpoint_tail(agent, &checkpoint_message);
        }
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
                    if let Some(text) = maybe_execute_slash_turn(
                        agent,
                        &mut output,
                        &settings,
                        &next_prompt,
                        &next_attachments,
                    ).await? {
                        Ok(text)
                    } else {
                        tracing::info!(chat_id = %chat_id, prompt_len = next_prompt.len(), atts = next_attachments.len(), "TURN_WORKER: calling agent.run");
                        let r = if next_attachments.is_empty() {
                            agent.run(&next_prompt, &mut output).await
                        } else {
                            let atts = std::mem::take(&mut next_attachments);
                            agent.run_with_attachments(&next_prompt, atts, &mut output).await
                        };
                        tracing::info!(chat_id = %chat_id, ok = r.is_ok(), "TURN_WORKER: agent.run returned");
                        r
                    }
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
                && let Err(e) = h.save(&chat_id, agent.messages())
            {
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
            if let Some(selection) = queued_model_selection(&drained) {
                if let Err(e) =
                    apply_model_selection_to_agent(agent, &state_for_task, &registry, &selection)
                {
                    tracing::warn!(error = %e, chat_id = %chat_id, "queued model switch failed");
                    chat_handle.emit(SseEvent::LlmError { message: e });
                }
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

    accepted_turn_response()
}

fn accepted_turn_response() -> Resp {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("Content-Type", "application/json")
        .body(boxed(Bytes::from_static(br#"{"ok":true}"#)))
        .unwrap()
}

fn spawn_direct_slash_turn(
    state: Arc<HttpState>,
    handle: Arc<ChatHandle>,
    chat_id: String,
    settings: Settings,
    prompt: String,
    attachments_present: bool,
) {
    let history = state.history.clone();
    let files = Arc::clone(&state.files);
    let file_id = Arc::clone(&state.file_id);
    let artefacts = Arc::clone(&state.artefacts);
    let artefact_id = Arc::clone(&state.artefact_id);
    let data_dir = state.data_dir.clone();
    let ingest = Arc::clone(&state.ingest);

    tokio::spawn(async move {
        let mut output = SseOutput {
            chat_id: chat_id.clone(),
            tx: handle.events.clone(),
            replay: Arc::clone(&handle.replay),
            files,
            next_file_id: file_id,
            artefacts,
            next_artefact_id: artefact_id,
            data_dir,
            pending: Some(Arc::clone(&handle)),
            current_tool_use_id: None,
            ingest,
        };

        let initial_messages = history
            .as_ref()
            .and_then(|h| h.load(&chat_id).ok())
            .unwrap_or_default();

        match crate::controller::slash::dispatch_without_llm(
            &mut output,
            &settings,
            &prompt,
            attachments_present,
            initial_messages,
        )
        .await
        {
            Ok(Some(messages)) => {
                if let Some(h) = history.as_ref()
                    && let Err(e) = h.save(&chat_id, &messages)
                {
                    tracing::warn!(error = %e, chat_id = %chat_id, "failed to save direct slash turn");
                }
            }
            Ok(None) => {
                tracing::warn!(chat_id = %chat_id, prompt = %prompt, "direct slash fast path had nothing to handle");
                handle.emit(SseEvent::LlmError {
                    message: "slash command was not handled".to_string(),
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %chat_id, "direct slash turn failed");
                handle.emit(SseEvent::LlmError {
                    message: e.sanitized_message(),
                });
            }
        }

        handle.emit(SseEvent::Done);
        handle.reset_replay();
        handle
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);
        bump_to_front(&state, &chat_id).await;
    });
}

async fn checkpoint_accepted_turn(
    handle: &Arc<ChatHandle>,
    history: Option<&Arc<dyn crate::chat_history::ChatHistory>>,
    chat_id: &str,
    checkpoint: &Message,
) -> crate::Result<bool> {
    let Some(history) = history else {
        return Ok(false);
    };

    let mut messages = {
        let guard = handle.agent.lock().await;
        match guard.as_ref() {
            Some(agent) => agent.messages().to_vec(),
            None => history.load(chat_id)?,
        }
    };
    messages.push(checkpoint.clone());
    history.save(chat_id, &messages)?;
    Ok(true)
}

fn accepted_turn_checkpoint_message(
    prompt: &str,
    attachments: &[crate::media::Attachment],
) -> Message {
    if attachments.is_empty() {
        return Message::user(prompt);
    }

    let mut blocks = Vec::new();
    if !prompt.is_empty() {
        blocks.push(ContentBlock::Text {
            text: prompt.to_string(),
        });
    }
    for attachment in attachments {
        let name = attachment
            .file_name
            .as_deref()
            .map(single_line_label)
            .unwrap_or_else(|| "attachment".to_string());
        blocks.push(ContentBlock::Text {
            text: format!(
                "[Attachment queued: {name} ({}, {} bytes)]",
                attachment.mime_type,
                attachment.data.len()
            ),
        });
    }
    Message::user_multimodal(blocks)
}

fn single_line_label(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn drop_checkpoint_tail(agent: &mut crate::agent::Agent, checkpoint: &Message) -> bool {
    let (messages, dropped) =
        drop_checkpoint_tail_from_messages(agent.messages().to_vec(), checkpoint);
    if dropped {
        agent.set_messages(messages);
    }
    dropped
}

fn drop_checkpoint_tail_from_messages(
    mut messages: Vec<Message>,
    checkpoint: &Message,
) -> (Vec<Message>, bool) {
    if !messages.last().is_some_and(|m| same_message(m, checkpoint)) {
        return (messages, false);
    }
    messages.pop();
    (messages, true)
}

async fn maybe_execute_slash_turn(
    agent: &mut crate::agent::Agent,
    output: &mut dyn Output,
    settings: &Settings,
    prompt: &str,
    attachments: &[crate::media::Attachment],
) -> crate::Result<Option<String>> {
    match crate::controller::slash::dispatch_executable(
        agent,
        output,
        settings,
        prompt,
        !attachments.is_empty(),
    )
    .await?
    {
        crate::controller::slash::SlashDispatch::Handled(text) => Ok(Some(text)),
        crate::controller::slash::SlashDispatch::NotSlash
        | crate::controller::slash::SlashDispatch::BuiltinOrUnhandled => Ok(None),
    }
}

fn same_message(a: &Message, b: &Message) -> bool {
    a.role == b.role && a.content == b.content
}

fn title_needs_generation(title: &str) -> bool {
    let t = title.trim();
    t.is_empty() || t.eq_ignore_ascii_case(PLACEHOLDER_TITLE)
}

fn spawn_title_generation(
    state: Arc<HttpState>,
    handle: Arc<ChatHandle>,
    chat_id: String,
    prompt: String,
    model: String,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
) {
    tokio::spawn(async move {
        match generate_title(client, model, prompt).await {
            Ok(title) => {
                handle.set_title(title.clone());
                if let Ok(mut titles) = state.titles.lock() {
                    titles.insert(chat_id.clone(), title.clone());
                }
                if let Some(h) = state.history.as_ref()
                    && let Err(e) = h.save_title(&chat_id, &title)
                {
                    tracing::warn!(error = %e, chat_id = %chat_id, "failed to save generated chat title");
                }
                handle.emit(SseEvent::Title { title });
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %chat_id, "background chat title generation failed");
            }
        }
    });
}

async fn generate_title(
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    model: String,
    prompt: String,
) -> crate::error::Result<String> {
    let config = CompletionConfig {
        model,
        // Reasoning models can spend hundreds of tokens on hidden
        // thinking before producing the short visible title.
        max_tokens: 1024,
        temperature: Some(0.2),
        api_tool_injections: Vec::new(),
    };
    let system = "You create concise chat titles. Return only a short title, 2-6 words, with no quotes, markdown, or trailing punctuation.";
    let user = format!(
        "Create a title for this new chat from the user's first message:\n\n{}",
        prompt.trim()
    );
    let messages = vec![Message::user(&user)];
    let response = client
        .access()?
        .stream(&messages, system, "", &[], &config)
        .await?;
    let mut stream = response.stream;
    let mut raw = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta(delta) => raw.push_str(&delta),
            StreamEvent::MessageComplete { .. } => break,
            StreamEvent::Error(e) => return Err(e),
            _ => {}
        }
    }
    clean_generated_title(&raw).ok_or_else(|| {
        crate::error::DysonError::Llm("title generation returned empty title".into())
    })
}

fn clean_generated_title(raw: &str) -> Option<String> {
    let mut title = raw.lines().find(|line| !line.trim().is_empty())?.trim();
    if let Some(rest) = title.strip_prefix("Title:") {
        title = rest.trim();
    } else if let Some(rest) = title.strip_prefix("title:") {
        title = rest.trim();
    }
    let mut title = title
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*' || c == '#')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    while matches!(
        title.chars().last(),
        Some('.' | '!' | '?' | ':' | ';' | ',')
    ) {
        title.pop();
    }
    if title.chars().count() > 64 {
        title = title.chars().take(64).collect::<String>();
        title = title.trim().to_string();
    }
    if title.is_empty() || title_needs_generation(&title) {
        None
    } else {
        Some(title)
    }
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
                Err(e) => {
                    tracing::warn!(error = %e, name = ?a.name, "queued attachment had malformed base64; skipping")
                }
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
    use super::super::super::state::{QueuedAttachment, QueuedTurn};
    use super::*;
    use crate::agent::rate_limiter::RateLimitedHandle;
    use crate::auth::Credential;
    use crate::llm::{LlmClient, ToolDefinition};
    use crate::sandbox::Sandbox;
    use crate::sandbox::no_sandbox::DangerousNoSandbox;
    use crate::skill::Skill;

    struct PanicLlm;

    #[async_trait::async_trait]
    impl LlmClient for PanicLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _system_suffix: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> crate::Result<crate::llm::StreamResponse> {
            panic!("slash skill dispatch must not call the LLM");
        }
    }

    #[test]
    fn coalesce_single_turn_passes_through() {
        let (prompt, atts) = coalesce_queued(vec![QueuedTurn {
            prompt: "hello".into(),
            attachments: vec![],
            provider: None,
            model: None,
            queue_mode: None,
        }]);
        assert_eq!(prompt, "hello");
        assert!(atts.is_empty());
    }

    #[tokio::test]
    async fn slash_dispatch_executes_skill_without_llm_call() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("skills/skill-echo");
        std::fs::create_dir_all(skill_dir.join("bin")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "Echoes input\n\nInstructions.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("bin/run.sh"), "cat\n").unwrap();
        std::fs::write(
            skill_dir.join("dyson-skill.json"),
            r#"{
              "schema_version": 2,
              "name": "skill-echo",
              "description": "Echoes input",
              "slash_command": "/skill-echo",
              "execution": { "kind": "script", "entrypoint": "bin/run.sh", "timeout_ms": 5000 }
            }"#,
        )
        .unwrap();

        let mut settings = Settings::default();
        settings.workspace.connection_string =
            Credential::new(tmp.path().to_string_lossy().to_string());
        let skills: Vec<Box<dyn Skill>> = vec![Box::new(
            crate::skill::local::LocalSkill::from_dir(&skill_dir).unwrap(),
        )];
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = crate::agent::Agent::new(
            RateLimitedHandle::unlimited(Box::new(PanicLlm)),
            sandbox,
            skills,
            &settings.agent,
            None,
            0,
            None,
            None,
        )
        .unwrap();
        let mut output = crate::controller::recording::RecordingOutput::new();

        let handled =
            maybe_execute_slash_turn(&mut agent, &mut output, &settings, "/skill-echo hello", &[])
                .await
                .unwrap();

        let text = handled.expect("slash command handled");
        assert!(text.contains("\"raw\":\"hello\""), "{text}");
        assert!(output.text().contains("\"raw\":\"hello\""));
        assert_eq!(agent.messages().len(), 2);
        assert!(matches!(
            agent.messages()[0].content.first(),
            Some(ContentBlock::Text { text }) if text == "/skill-echo hello"
        ));
    }

    #[tokio::test]
    async fn unknown_slash_command_is_not_sent_to_llm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.workspace.connection_string =
            Credential::new(tmp.path().to_string_lossy().to_string());
        let sandbox: Arc<dyn Sandbox> = Arc::new(DangerousNoSandbox);
        let mut agent = crate::agent::Agent::new(
            RateLimitedHandle::unlimited(Box::new(PanicLlm)),
            sandbox,
            Vec::new(),
            &settings.agent,
            None,
            0,
            None,
            None,
        )
        .unwrap();
        let mut output = crate::controller::recording::RecordingOutput::new();

        let handled = maybe_execute_slash_turn(&mut agent, &mut output, &settings, "/wat", &[])
            .await
            .unwrap();

        let text = handled.expect("unknown slash command handled");
        assert!(text.contains("Unknown slash command '/wat'"));
        assert!(output.text().contains("Unknown slash command"));
        assert_eq!(agent.messages().len(), 2);
    }

    #[test]
    fn coalesce_multi_turn_numbers_with_preamble() {
        let (prompt, _) = coalesce_queued(vec![
            QueuedTurn {
                prompt: "first".into(),
                attachments: vec![],
                provider: None,
                model: None,
                queue_mode: None,
            },
            QueuedTurn {
                prompt: "second".into(),
                attachments: vec![],
                provider: None,
                model: None,
                queue_mode: None,
            },
            QueuedTurn {
                prompt: "third".into(),
                attachments: vec![],
                provider: None,
                model: None,
                queue_mode: None,
            },
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
                provider: None,
                model: None,
                queue_mode: None,
            },
            QueuedTurn {
                prompt: "b".into(),
                attachments: vec![QueuedAttachment {
                    mime_type: "image/png".into(),
                    name: Some("two.png".into()),
                    data_base64: b64(b"\x89PNG two"),
                }],
                provider: None,
                model: None,
                queue_mode: None,
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
            provider: None,
            model: None,
            queue_mode: None,
        }]);
        assert!(
            atts.is_empty(),
            "malformed attachment must be skipped, not panic"
        );
    }

    #[test]
    fn clean_generated_title_strips_model_wrapping() {
        assert_eq!(
            clean_generated_title("Title: \"Investigate Login Failure.\"\n").as_deref(),
            Some("Investigate Login Failure")
        );
        assert_eq!(clean_generated_title("New conversation"), None);
    }

    #[test]
    fn accepted_turn_checkpoint_records_attachment_placeholders() {
        let msg = accepted_turn_checkpoint_message(
            "inspect this",
            &[crate::media::Attachment {
                data: b"hello".to_vec(),
                mime_type: "text/plain".into(),
                file_name: Some("notes\n.txt".into()),
            }],
        );
        assert_eq!(msg.role, crate::message::Role::User);
        assert_eq!(
            msg.content,
            vec![
                ContentBlock::Text {
                    text: "inspect this".into()
                },
                ContentBlock::Text {
                    text: "[Attachment queued: notes .txt (text/plain, 5 bytes)]".into()
                },
            ]
        );
    }

    #[test]
    fn checkpoint_tail_drop_removes_only_the_provisional_tail() {
        let checkpoint = Message::user("still running");
        let (messages, dropped) = drop_checkpoint_tail_from_messages(
            vec![Message::user("before"), checkpoint.clone()],
            &checkpoint,
        );
        assert!(dropped);
        assert_eq!(messages.len(), 1);
        assert!(same_message(&messages[0], &Message::user("before")));

        let (messages, dropped) =
            drop_checkpoint_tail_from_messages(vec![Message::user("before")], &checkpoint);
        assert!(!dropped);
        assert_eq!(messages.len(), 1);
        assert!(same_message(&messages[0], &Message::user("before")));
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

        let mut reloader = crate::config::hot_reload::HotReloader::new(Some(&cfg), None);

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
