//! Telegram update delivery and controller lifecycle.
//!
//! Configuration, per-chat state, callbacks, commands, turns, message addressing,
//! attachments, and output rendering live in focused sibling modules.

mod api;
mod formatting;
pub mod output;
pub mod types;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use self::types::{ChatId, Update};
use tokio::sync::mpsc;

use crate::config::Settings;

pub use self::formatting::is_public_command;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

use crate::controller::WARMUP_PLACEHOLDER;

/// Minimum interval between message edits (milliseconds).
const EDIT_INTERVAL_MS: u128 = 500;

/// Maximum message length for Telegram (UTF-8 characters).
const MAX_MESSAGE_LEN: usize = 4000;

/// Maximum number of concurrent chat entries kept in memory.
/// When exceeded, the least-recently-active entry is evicted (its
/// conversation is already persisted to chat_store on every turn).
const MAX_CHAT_ENTRIES: usize = 200;

/// Maximum number of in-flight tasks (agent run + quick response) per chat.
/// One real agent run plus a couple of quick-response fallbacks is plenty;
/// any more is a hostile or buggy client and the extra messages are dropped
/// without spawning.  Keeps memory linear in the number of active chats
/// instead of the number of messages received.
const MAX_IN_FLIGHT_PER_CHAT: usize = 3;

static TELEGRAM_WEBHOOK_SENDER: OnceLock<RwLock<Option<mpsc::Sender<Update>>>> = OnceLock::new();

pub async fn enqueue_webhook_update(update: Update) -> Result<(), &'static str> {
    let sender = TELEGRAM_WEBHOOK_SENDER
        .get()
        .and_then(|slot| slot.read().ok().and_then(|guard| guard.clone()))
        .ok_or("telegram webhook receiver is not running")?;
    sender
        .send(update)
        .await
        .map_err(|_| "telegram webhook receiver is closed")
}

fn install_webhook_sender(sender: mpsc::Sender<Update>) {
    let slot = TELEGRAM_WEBHOOK_SENDER.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = slot.write() {
        *guard = Some(sender);
    }
}

fn clear_webhook_sender() {
    if let Some(slot) = TELEGRAM_WEBHOOK_SENDER.get()
        && let Ok(mut guard) = slot.write()
    {
        *guard = None;
    }
}

/// Current Unix timestamp in seconds (for `last_active` bookkeeping).
fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// TelegramController
// ---------------------------------------------------------------------------

/// Telegram bot controller.
pub struct TelegramController {
    /// Bot API token.  Uses `Credential` for zeroize-on-drop.
    bot_token: Option<crate::auth::Credential>,
    proxy: Option<TelegramProxyConfig>,
    mode: TelegramMode,
    allowed_chat_ids: Vec<i64>,
    download_limits: Arc<DownloadLimits>,
}

#[async_trait::async_trait]
impl super::Controller for TelegramController {
    fn name(&self) -> &str {
        "telegram"
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(
            "You are responding via Telegram. Keep these rules:\n\
             - Keep responses concise. Telegram messages have a 4096 character limit.\n\
             - Use line breaks for readability.\n\
             - Markdown formatting is fine — it will be converted automatically.\n\
             - You can send files to the user. When a tool produces a file, it will \
             be delivered as a Telegram document automatically.",
        )
    }

    async fn run(
        &self,
        settings: &Settings,
        registry: &std::sync::Arc<super::ClientRegistry>,
    ) -> crate::Result<()> {
        eprintln!(
            "Dyson v{} — running as Telegram bot",
            env!("CARGO_PKG_VERSION")
        );

        let mut active_config = Self {
            bot_token: self
                .bot_token
                .as_ref()
                .map(|token| crate::auth::Credential::new(token.expose().to_owned())),
            proxy: self.proxy.clone(),
            mode: self.mode,
            allowed_chat_ids: self.allowed_chat_ids.clone(),
            download_limits: Arc::clone(&self.download_limits),
        };
        let mut bot = active_config.build_bot();

        // Fetch the bot's own identity so we can filter group messages
        // to only those that @mention or reply to the bot.
        let mut bot_username = String::new();
        let mut bot_id = 0;
        match bot.get_me().await {
            Ok(me) => {
                bot_username = me.username.unwrap_or_default().to_lowercase();
                bot_id = me.id;
            }
            Err(err) if active_config.mode == TelegramMode::Webhook => {
                tracing::warn!(
                    error = %err,
                    "Telegram webhook mode started before proxy is ready; will retry after configure"
                );
            }
            Err(err) => return Err(err),
        }
        if bot_username.is_empty() {
            tracing::warn!("getMe returned no username — group mention filtering will not work");
        } else {
            tracing::info!(bot_username = bot_username.as_str(), "bot identity fetched");
        }

        let mut allowed_ids = self.allowed_chat_ids.clone();
        let mut download_limits = Arc::clone(&self.download_limits);
        let mut current_settings = settings.clone();
        let controller_prompt = self.system_prompt().map(std::string::ToString::to_string);

        // Resolve the config path for `/model` persistence only — the
        // actual file watcher has moved to `command::listen` so the
        // registry reload + settings broadcast happens once per
        // process regardless of how many controllers subscribe.
        // Telegram now subscribes to the shared bus and limits itself
        // to rebuilding its own agents when a change arrives.
        let (config_path, _unused_reloader) = super::create_hot_reloader(settings, None);
        let mut settings_rx = super::subscribe_settings_updates();

        let bg_registry = std::sync::Arc::new(super::background::BackgroundAgentRegistry::new());

        let agents: Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        let chat_store: Arc<dyn crate::chat_history::ChatHistory> = {
            let store = crate::chat_history::create_chat_history(&settings.chat_history)?;
            Arc::from(store)
        };

        // Feedback directory — lives alongside chat history.  Each agent gets
        // its own FeedbackStore instance via set_feedback_store().
        let feedback_dir =
            crate::util::resolve_tilde(settings.chat_history.connection_string.expose());

        let mut offset: i64 = 0;
        let mut consecutive_failures: u64 = 0;
        let mut backoff_secs: u64 = 1;
        let mut webhook_rx = if active_config.mode == TelegramMode::Webhook {
            let (tx, rx) = mpsc::channel(256);
            install_webhook_sender(tx);
            Some(rx)
        } else {
            None
        };

        loop {
            // Pull any published settings change since the last
            // iteration.  `has_changed` is cheap (one atomic load);
            // `borrow_and_update` marks the latest value as seen so
            // the next check doesn't retrigger until the program
            // broadcaster sends again.  Registry + config-file
            // reload already happened centrally before this fires.
            if let Some(rx) = settings_rx.as_mut()
                && rx.has_changed().unwrap_or(false)
            {
                let fresh = rx.borrow_and_update().clone();
                current_settings = (*fresh).clone();
                if let Some(updated) = Self::from_settings(&current_settings) {
                    active_config = updated;
                    bot = active_config.build_bot();
                    allowed_ids = active_config.allowed_chat_ids.clone();
                    download_limits = Arc::clone(&active_config.download_limits);
                    match bot.get_me().await {
                        Ok(me) => {
                            bot_username = me.username.unwrap_or_default().to_lowercase();
                            bot_id = me.id;
                            tracing::info!(
                                bot_username = bot_username.as_str(),
                                "telegram bot identity refreshed after settings reload"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "telegram getMe failed after settings reload"
                            );
                        }
                    }
                }
                rebuild_agents_on_reload(
                    &agents,
                    &current_settings,
                    controller_prompt.as_deref(),
                    &chat_store,
                    &feedback_dir,
                    registry,
                )
                .await;
            }

            let updates = if let Some(rx) = webhook_rx.as_mut() {
                tokio::select! {
                    update = rx.recv() => match update {
                        Some(update) => vec![update],
                        None => break,
                    },
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("\nshutting down");
                        break;
                    }
                }
            } else {
                // Poll for updates with a timeout, racing against Ctrl-C.
                tokio::select! {
                    result = bot.get_updates(offset, 30) => {
                        match result {
                            Ok(updates) => {
                                if consecutive_failures > 0 {
                                    tracing::info!(
                                        consecutive_failures,
                                        "getUpdates recovered after network errors",
                                    );
                                }
                                consecutive_failures = 0;
                                backoff_secs = 1;
                                updates
                            }
                            Err(e) => {
                                consecutive_failures += 1;
                                if consecutive_failures.is_multiple_of(30) {
                                    tracing::warn!(
                                        error = %e,
                                        consecutive_failures,
                                        "getUpdates has been failing for a while",
                                    );
                                } else {
                                    tracing::debug!(error = %e, consecutive_failures, "getUpdates failed — retrying");
                                }
                                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                                backoff_secs = (backoff_secs * 2).min(60);
                                continue;
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("\nshutting down");
                        break;
                    }
                }
            };

            // A webhook-mode controller can sit parked on rx.recv()
            // indefinitely. If swarm reconfigured the warmup cube while
            // we were waiting, consume that settings update before the
            // first Telegram update builds a per-chat agent.
            if let Some(rx) = settings_rx.as_mut()
                && rx.has_changed().unwrap_or(false)
            {
                let fresh = rx.borrow_and_update().clone();
                current_settings = (*fresh).clone();
                if let Some(updated) = Self::from_settings(&current_settings) {
                    active_config = updated;
                    bot = active_config.build_bot();
                    allowed_ids = active_config.allowed_chat_ids.clone();
                    download_limits = Arc::clone(&active_config.download_limits);
                    match bot.get_me().await {
                        Ok(me) => {
                            bot_username = me.username.unwrap_or_default().to_lowercase();
                            bot_id = me.id;
                            tracing::info!(
                                bot_username = bot_username.as_str(),
                                "telegram bot identity refreshed after settings reload"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "telegram getMe failed after settings reload"
                            );
                        }
                    }
                }
                rebuild_agents_on_reload(
                    &agents,
                    &current_settings,
                    controller_prompt.as_deref(),
                    &chat_store,
                    &feedback_dir,
                    registry,
                )
                .await;
            }

            for update in &updates {
                offset = update.update_id + 1;

                // Handle emoji reactions for feedback/RLHF signal.
                if let Some(reaction) = &update.message_reaction {
                    handle_reaction(reaction, &agents).await;
                    continue;
                }

                if let Some(cb) = &update.callback_query {
                    let cb_chat = cb.message.as_ref().map(|m| &m.chat);
                    let cb_chat_id = cb_chat.map(|c| ChatId(c.id));
                    let cb_is_group = cb_chat.is_some_and(types::Chat::is_group);
                    let cb_data = cb.data.clone().unwrap_or_default();
                    let _ = bot.answer_callback_query(&cb.id).await;

                    if let Some(chat_id) = cb_chat_id {
                        if should_reject(cb_is_group, &allowed_ids, chat_id.0) {
                            continue;
                        }
                        handle_callback_query(
                            &bot,
                            &cb_data,
                            chat_id,
                            &agents,
                            &current_settings,
                            config_path.as_deref(),
                            registry,
                            &bg_registry,
                        )
                        .await;
                    }
                    continue;
                }

                let msg = match update
                    .message
                    .as_ref()
                    .or(update.edited_message.as_ref())
                    .or(update.channel_post.as_ref())
                {
                    Some(m) => m.clone(),
                    None => continue,
                };

                let text = msg
                    .text
                    .as_deref()
                    .or(msg.caption.as_deref())
                    .filter(|t| !t.is_empty())
                    .map(std::string::ToString::to_string);

                // Any document counts as media here, even unsupported ones:
                // we want to send the user a "skipped" reply rather than
                // silently dropping the message.
                let has_media =
                    msg.photo.is_some() || msg.voice.is_some() || msg.document.is_some();

                if text.is_none() && !has_media {
                    continue;
                }

                let text = strip_bot_mention(&text.unwrap_or_default(), &bot_username);
                let chat_id = ChatId(msg.chat.id);

                if is_public_command(&text) {
                    let _ = bot.send_message(chat_id, &chat_id.0.to_string()).await;
                    continue;
                }

                let is_group = msg.chat.is_group();

                // Group chats are always allowed — they run as public agents
                // with restricted tools, so they're safe without whitelisting.
                // Private chats require explicit allowed_chat_ids.
                if should_reject(is_group, &allowed_ids, chat_id.0) {
                    tracing::warn!(chat_id = chat_id.0, "unauthorized private chat — ignoring");
                    continue;
                }

                if is_group && !is_directed_group_msg(&msg, &text, &bot_username, bot_id) {
                    continue;
                }
                let sender_id = msg.from.as_ref().map(|u| u.id);
                if should_reject_group_command(is_group, &text, sender_id, &allowed_ids) {
                    tracing::info!(
                        chat_id = chat_id.0,
                        sender_id = ?sender_id,
                        command = %text,
                        "non-operator command in group — ignoring",
                    );
                    continue;
                }

                if handle_instant_command(
                    &bot,
                    &text,
                    chat_id,
                    &current_settings,
                    registry,
                    &bg_registry,
                    &agents,
                    &chat_store,
                )
                .await
                {
                    continue;
                }

                if text == "/clear" || text == "/compact" || text.starts_with("/model ") {
                    let chat_cx = ChatContext {
                        settings: &current_settings,
                        controller_prompt: controller_prompt.as_deref(),
                        chat_store: &chat_store,
                        feedback_dir: &feedback_dir,
                        registry,
                    };
                    let entry =
                        match get_or_create_entry(&agents, chat_id.0, is_group, &chat_cx).await {
                            Ok(e) => e,
                            Err(e) => {
                                let _ = bot.send_message(chat_id, &format!("Error: {e}")).await;
                                continue;
                            }
                        };
                    handle_per_chat_command(
                        &bot,
                        &text,
                        chat_id,
                        &entry,
                        &current_settings,
                        config_path.as_deref(),
                        &*chat_store,
                        registry,
                    )
                    .await;
                    continue;
                }

                tracing::info!(chat_id = chat_id.0, is_group, "telegram message received");

                let chat_cx = ChatContext {
                    settings: &current_settings,
                    controller_prompt: controller_prompt.as_deref(),
                    chat_store: &chat_store,
                    feedback_dir: &feedback_dir,
                    registry,
                };
                let entry = match get_or_create_entry(&agents, chat_id.0, is_group, &chat_cx).await
                {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = bot.send_message(chat_id, &format!("Error: {e}")).await;
                        continue;
                    }
                };

                // Bound in-flight work per chat.  A single user flooding
                // messages cannot spawn more than MAX_IN_FLIGHT_PER_CHAT
                // concurrent tasks — extras are dropped (with a log) rather
                // than queued.  The permit lives for the task's duration and
                // is released automatically on drop.
                let permit = match Arc::clone(&entry.in_flight).try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        tracing::warn!(
                            chat_id = chat_id.0,
                            "dropping telegram message: in-flight limit reached"
                        );
                        continue;
                    }
                };

                let bot_clone = bot.clone();
                let store_clone = chat_store.clone();
                let client_for_task = registry.get_default();
                let limits_clone = Arc::clone(&download_limits);
                let settings_for_task = current_settings.clone();
                tokio::spawn(async move {
                    run_agent_for_message(
                        bot_clone,
                        chat_id,
                        msg,
                        text,
                        entry,
                        store_clone,
                        client_for_task,
                        limits_clone,
                        settings_for_task,
                    )
                    .await;
                    drop(permit);
                });
            }
        }

        clear_webhook_sender();
        Ok(())
    }
}

/// Rebuild all per-chat agents after a config/workspace reload, preserving
/// each chat's provider/model selection and conversation history.
// TelegramOutput and formatting helpers are in submodules:
// - output.rs    — TelegramOutput struct and Output trait impl
// - formatting.rs — markdown-to-HTML conversion, message splitting, tests
mod chats;
use chats::{ChatContext, ChatEntry, get_or_create_entry, rebuild_agents_on_reload};

mod config;
use config::{TelegramMode, TelegramProxyConfig};

mod callbacks;
use callbacks::{handle_callback_query, handle_reaction};

mod commands;
use commands::{handle_instant_command, handle_per_chat_command};

mod turns;
use turns::run_agent_for_message;

mod messages;
pub use messages::is_operator;
use messages::{is_directed_group_msg, should_reject, should_reject_group_command};

mod attachments;

use dyson_telegram::media::DownloadLimits;
use formatting::strip_bot_mention;

#[cfg(test)]
mod tests;
