//! Telegram command execution, result rendering, and inline keyboards.
use super::{
    ChatEntry,
    api::BotApi,
    formatting::format_logs_for_telegram,
    output::TelegramOutput,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup},
};
use crate::config::Settings;
use std::{collections::HashMap, sync::Arc};

/// Drop guard that returns a temporarily-extracted agent to its `ChatEntry`
/// mutex.  If the normal put-back path runs, `agent` is `take()`-n out of the
/// guard first, making `Drop` a no-op.  On panic, `Drop` fires and the agent
/// is returned via `try_lock()` — blocking `.lock().await` isn't available in
/// a synchronous `Drop`, but `try_lock()` will succeed because the panic
/// unwinds through the only code path that holds the extracted agent.
struct AgentGuard {
    entry: Arc<ChatEntry>,
    agent: Option<crate::agent::Agent>,
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            // Best-effort: return the agent so the chat isn't permanently broken.
            if let Ok(mut ca) = self.entry.agent.try_lock() {
                ca.agent = Some(agent);
            } else {
                tracing::error!("AgentGuard::drop — could not reacquire lock to return agent");
            }
        }
    }
}

/// Handle per-chat commands (/clear, /compact, /model) that need the agent lock.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_per_chat_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    entry: &Arc<ChatEntry>,
    settings: &Settings,
    config_path: Option<&std::path::Path>,
    chat_store: &dyn crate::chat_history::ChatHistory,
    registry: &crate::controller::ClientRegistry,
) {
    // Extract agent from the mutex so execute_agent_command (which may do an
    // LLM call for /compact) runs without holding the lock.
    //
    // AgentGuard ensures the agent is put back even if a panic occurs.
    let mut ca = entry.agent.lock().await;
    let agent = ca.agent.take().expect("agent not available");
    let mut provider_name = ca.provider_name.clone();
    let mut model = ca.model.clone();
    drop(ca); // Release lock before command execution.

    let mut guard = AgentGuard {
        entry: Arc::clone(entry),
        agent: Some(agent),
    };
    let agent = guard.agent.as_mut().expect("just set");

    let mut output = TelegramOutput::new(bot.clone(), chat_id, true);
    let result = crate::controller::execute_agent_command(
        text,
        agent,
        &mut output,
        settings,
        &mut crate::controller::ModelState {
            provider: &mut provider_name,
            model: &mut model,
            config_path,
        },
        registry,
    )
    .await;

    // Snapshot state from the agent while we still own it (no lock needed).
    let agent = guard.agent.as_ref().expect("still held");
    let (snapshot_msgs, snapshot_prompt, snapshot_config) = match &result {
        crate::controller::CommandResult::Compacted => {
            (Some(agent.messages().to_vec()), None, None)
        }
        crate::controller::CommandResult::ModelSwitched { .. } => (
            None,
            Some(agent.system_prompt().to_string()),
            Some(agent.config().clone()),
        ),
        _ => (None, None, None),
    };

    // Put the agent back and update provider/model if changed.
    {
        let agent = guard.agent.take().expect("still held");
        let mut ca = entry.agent.lock().await;
        ca.agent = Some(agent);
        ca.provider_name = provider_name;
        ca.model = model;
    }

    // Side effects that need entry/chat_store access.
    match &result {
        crate::controller::CommandResult::Cleared => {
            *entry.messages_snapshot.write().await = Vec::new();
            entry.message_id_map.write().await.clear();
            let _ = chat_store.rotate(&chat_id.0.to_string());
            tracing::info!(chat_id = chat_id.0, "conversation rotated and cleared");
        }
        crate::controller::CommandResult::Compacted => {
            let msgs = snapshot_msgs.expect("set above for Compacted");
            if let Err(e) = chat_store.save(&chat_id.0.to_string(), &msgs) {
                tracing::error!(error = %e, "failed to save chat history");
            }
            *entry.messages_snapshot.write().await = msgs;
            tracing::info!(chat_id = chat_id.0, "conversation compacted");
        }
        crate::controller::CommandResult::ModelSwitched { .. } => {
            if let Some(prompt) = snapshot_prompt {
                *entry.system_prompt.write().await = prompt;
            }
            if let Some(config) = snapshot_config {
                *entry.config.write().await = config;
            }
        }
        _ => {}
    }

    render_command_result_telegram(bot, chat_id, &result).await;
}

/// Handle instant commands that don't need the agent lock.
///
/// Returns whether the command was handled.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_instant_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    settings: &Settings,
    registry: &crate::controller::ClientRegistry,
    bg_registry: &std::sync::Arc<crate::controller::background::BackgroundAgentRegistry>,
    agents: &Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
) -> bool {
    // Telegram-only commands not in the shared dispatcher.
    if text == "/memory" {
        let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
        return true;
    }
    if let Some(note) = text.strip_prefix("/memory ") {
        let note = note.trim();
        if note.is_empty() {
            let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
            return true;
        }
        match save_memory_note(settings, note) {
            Ok(()) => {
                let _ = bot.send_message(chat_id, "Saved to memory.").await;
                tracing::info!(chat_id = chat_id.0, "memory note saved");
            }
            Err(e) => {
                let _ = bot.send_message(chat_id, &format!("Error: {e}")).await;
                tracing::error!(error = %e, "failed to save memory note");
            }
        }
        return true;
    }

    // /models gets special treatment in Telegram (inline keyboard).
    if text == "/models" {
        handle_models_command(bot, chat_id, settings).await;
        return true;
    }
    if text == "/model" {
        let _ = bot
            .send_message(
                chat_id,
                "Usage: /model <provider> [model]  or  /model <model>",
            )
            .await;
        return true;
    }

    // Shared lock-free commands.  Background agents deliver their final
    // response back to the originating Telegram chat via the on_complete
    // callback — both as a visible message and as an appended turn in the
    // chat's persisted conversation history so the next user turn sees it.
    let bot_clone = bot.clone();
    let agents_clone = Arc::clone(agents);
    let store_clone = Arc::clone(chat_store);
    let origin_chat = chat_id;
    let on_complete: crate::controller::BackgroundCompletion =
        std::sync::Arc::new(move |id, result| {
            let bot = bot_clone.clone();
            let agents = Arc::clone(&agents_clone);
            let store = Arc::clone(&store_clone);
            tokio::spawn(async move {
                let formatted = crate::controller::format_background_result(id, &result);
                for part in super::formatting::split_for_telegram(&formatted) {
                    let _ = bot.send_message(origin_chat, &part).await;
                }
                if let Err(e) =
                    append_background_result_to_chat(&agents, &*store, origin_chat, id, &result)
                        .await
                {
                    tracing::warn!(
                        chat_id = origin_chat.0,
                        agent_id = id,
                        error = %e,
                        "failed to append background result to chat history",
                    );
                }
            });
        });
    let result = crate::controller::execute_lockfree_command(
        text,
        settings,
        registry,
        bg_registry,
        Some(on_complete),
    )
    .await;
    if matches!(result, crate::controller::CommandResult::NotHandled) {
        return false;
    }
    render_command_result_telegram(bot, chat_id, &result).await;
    true
}

/// Append a finished background agent's result to the originating chat.
///
/// When the chat has a live in-memory agent, we mutate its conversation in
/// place (under the per-chat mutex) so the next user turn sees the result
/// without a reload.  The on-disk copy and the quick-response snapshot are
/// refreshed from the same message vector.
///
/// When no entry exists yet (e.g. the user's first message in this session
/// was `/loop`), we fall back to persisting via `chat_store` only — the
/// entry will restore from disk when it's lazily created.
async fn append_background_result_to_chat(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    chat_store: &dyn crate::chat_history::ChatHistory,
    chat_id: ChatId,
    id: u64,
    result: &std::result::Result<String, String>,
) -> crate::Result<()> {
    let chat_key = chat_id.0.to_string();
    let entry = agents.read().await.get(&chat_id.0).cloned();
    let Some(entry) = entry else {
        crate::controller::persist_background_result(chat_store, &chat_key, id, result)?;
        return Ok(());
    };

    let mut ca = entry.agent.lock().await;
    let msgs = if let Some(agent) = ca.agent.as_mut() {
        let mut msgs = agent.messages().to_vec();
        msgs.push(crate::message::Message::user(
            &crate::controller::format_background_result(id, result),
        ));
        agent.set_messages(msgs.clone());
        msgs
    } else {
        // Agent temporarily extracted (e.g. during /compact) — persist via
        // chat_store and let the rebuild pick it up.
        drop(ca);
        crate::controller::persist_background_result(chat_store, &chat_key, id, result)?;
        return Ok(());
    };
    drop(ca);

    chat_store.save(&chat_key, &msgs)?;
    *entry.messages_snapshot.write().await = msgs;
    Ok(())
}

/// Render a `CommandResult` as a Telegram message.
async fn render_command_result_telegram(
    bot: &BotApi,
    chat_id: ChatId,
    result: &crate::controller::CommandResult,
) {
    match result {
        crate::controller::CommandResult::Cleared => {
            let _ = bot.send_message(chat_id, "Context cleared.").await;
        }
        crate::controller::CommandResult::Compacted => {
            let _ = bot.send_message(chat_id, "Context compacted.").await;
        }
        crate::controller::CommandResult::CompactError(e) => {
            let _ = bot
                .send_message(chat_id, &format!("Compaction failed: {e}"))
                .await;
        }
        crate::controller::CommandResult::ModelSwitched {
            provider_name,
            provider_type,
            model,
        } => {
            let _ = bot
                .send_message(
                    chat_id,
                    &format!("Switched to '{provider_name}' — {provider_type} ({model})"),
                )
                .await;
        }
        crate::controller::CommandResult::ModelSwitchError(e) => {
            let _ = bot
                .send_message(chat_id, &format!("Switch error: {e}"))
                .await;
        }
        crate::controller::CommandResult::ModelParseError(e) => {
            let _ = bot.send_message(chat_id, e).await;
        }
        crate::controller::CommandResult::ModelUsage => {
            let _ = bot
                .send_message(
                    chat_id,
                    "Usage: /model <provider> [model]  or  /model <model>",
                )
                .await;
        }
        crate::controller::CommandResult::Logs(lines) => {
            for part in format_logs_for_telegram(lines) {
                let _ = bot.send_message_html(chat_id, &part).await;
            }
        }
        crate::controller::CommandResult::LogsError(e) => {
            let _ = bot.send_message(chat_id, &format!("Logs error: {e}")).await;
        }
        crate::controller::CommandResult::LoopStarted {
            id,
            chat_id: bg_chat_id,
            ..
        } => {
            let _ = bot
                .send_message(
                    chat_id,
                    &format!("Agent #{id} started — chat: {bg_chat_id}"),
                )
                .await;
        }
        crate::controller::CommandResult::LoopError(e) => {
            let _ = bot.send_message(chat_id, &format!("Loop error: {e}")).await;
        }
        crate::controller::CommandResult::AgentList { agents } => {
            if agents.is_empty() {
                let _ = bot
                    .send_message(chat_id, "No background agents running.")
                    .await;
            } else {
                let keyboard = build_agents_keyboard(agents);
                let _ = bot
                    .send_message_with_keyboard(
                        chat_id,
                        "Background agents — tap to stop:",
                        &keyboard,
                    )
                    .await;
            }
        }
        crate::controller::CommandResult::AgentStopped { id } => {
            let _ = bot
                .send_message(chat_id, &format!("Agent #{id} stopped."))
                .await;
        }
        crate::controller::CommandResult::StopError(e) => {
            let _ = bot.send_message(chat_id, &format!("Stop error: {e}")).await;
        }
        crate::controller::CommandResult::ModelList { .. }
        | crate::controller::CommandResult::NotHandled => {}
    }
}

/// Handle the /models command — show an inline keyboard with all providers/models.
async fn handle_models_command(bot: &BotApi, chat_id: ChatId, settings: &Settings) {
    if settings.providers.is_empty() {
        let _ = bot.send_message(chat_id, "No providers configured.").await;
        return;
    }

    let current_provider = crate::controller::active_provider_name(settings).unwrap_or_default();
    let current_model = &settings.agent.model;
    let providers: Vec<crate::controller::ProviderInfo> =
        crate::controller::list_providers(settings)
            .into_iter()
            .map(|(name, pc)| crate::controller::ProviderInfo {
                name: name.to_string(),
                provider_type: format!("{:?}", pc.provider_type),
                models: pc
                    .models
                    .iter()
                    .map(|m| crate::controller::ModelInfo {
                        name: m.clone(),
                        active: name == current_provider.as_str() && m == current_model,
                    })
                    .collect(),
            })
            .collect();
    let keyboard = build_model_keyboard(&providers);
    let _ = bot
        .send_message_with_keyboard(chat_id, "Select a model:", &keyboard)
        .await;
}

/// Build an inline keyboard listing all providers and models.
///
/// Each button shows the model name (with a check mark for the active one).
/// The callback data encodes `model:{provider}:{model}` so the handler
/// can switch to the selected model.
pub(super) fn build_model_keyboard(
    providers: &[crate::controller::ProviderInfo],
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for provider in providers {
        // Provider header row (non-clickable label).
        let label = format!("{} — {}", provider.name, provider.provider_type);
        rows.push(vec![InlineKeyboardButton::callback(label, "noop")]);

        for model in &provider.models {
            let display = if model.active {
                format!("✓ {}", model.name)
            } else {
                model.name.clone()
            };
            let data = format!("model:{}:{}", provider.name, model.name);
            rows.push(vec![InlineKeyboardButton::callback(display, data)]);
        }
    }

    InlineKeyboardMarkup::new(rows)
}

/// Build an inline keyboard listing all running background agents as
/// tap-to-stop buttons.
///
/// Each button shows `⏹ Stop #{id}: {preview} ({elapsed}s)`.  The callback
/// data encodes `stop_agent:{id}` so the handler can cancel the selected
/// agent via the `BackgroundAgentRegistry`.
pub(super) fn build_agents_keyboard(
    agents: &[crate::controller::background::BackgroundAgentListEntry],
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for a in agents {
        // Telegram inline buttons cap at ~64 chars — keep the preview short
        // so the "Stop" label stays visible.
        let preview = truncate_chars(&a.prompt_preview, 32);
        let display = format!(
            "⏹ Stop #{id}: {preview} ({elapsed:.0}s)",
            id = a.id,
            elapsed = a.elapsed.as_secs_f64(),
        );
        let data = format!("stop_agent:{}", a.id);
        rows.push(vec![InlineKeyboardButton::callback(display, data)]);
    }
    InlineKeyboardMarkup::new(rows)
}

/// Truncate a string to at most `max` chars, appending `…` if truncated.
pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Save a note to the workspace MEMORY.md file.
fn save_memory_note(settings: &Settings, note: &str) -> crate::Result<()> {
    let mut workspace = crate::workspace::create_workspace(&settings.workspace)?;

    let today = crate::workspace::FilesystemWorkspace::today_date();
    let entry = format!("\n- [{today}] {note}");

    workspace.append("MEMORY.md", &entry);
    workspace.save()?;

    Ok(())
}
