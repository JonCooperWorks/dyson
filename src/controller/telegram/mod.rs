// ===========================================================================
// Telegram controller — run Dyson as a Telegram bot.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the `Controller` trait for Telegram.  Polls for incoming
//   messages, runs the agent for each one, and streams the response back
//   by editing a Telegram message as tokens arrive.
//
// How Telegram "streaming" works:
//   Telegram doesn't support true token-by-token streaming.  We simulate it:
//   1. On the first TextDelta, send a new message with the text so far.
//   2. On subsequent TextDeltas, edit that message with accumulated text.
//   3. Throttle edits to EDIT_INTERVAL_MS (500ms) to avoid rate limits.
//
// Architecture:
//
//   TelegramController::run()
//     │
//     ├── create BotApi from bot_token
//     ├── polling loop (getUpdates):
//     │     ├── receive Message from Telegram
//     │     ├── check allowed_chat_ids (access control)
//     │     ├── lock-free commands (/models, /logs) → respond instantly
//     │     ├── per-chat commands (/clear, /model) → per-chat lock
//     │     └── agent messages → spawn background task:
//     │           ├── if agent busy → quick_response (no tools, fast)
//     │           └── if agent free → agent.run(text, &mut output)
//     │                 ├── output.text_delta("Hello") → edit message
//     │                 └── output.flush()             → final edit
//     └── runs until shutdown
//
// Concurrency model:
//   Each chat has its own ChatEntry with an independent Mutex.  The map
//   of entries uses RwLock — lookups take a read lock (non-blocking),
//   insertions take a write lock (brief).  This means:
//   - Different chats never block each other.
//   - Commands like /models that don't need the agent respond instantly.
//   - When a chat's agent is busy, new messages get quick responses
//     (single LLM call, no tools) instead of blocking.
//
// Async bridging:
//   The `Output` trait is sync (for terminal compatibility).  TelegramOutput
//   uses a channel-based design: sync methods send events through an mpsc
//   channel, and a background task consumes them with async API calls.
//   This avoids block_in_place and works with current_thread tokio.
// ===========================================================================

mod api;
mod formatting;
pub mod output;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use self::api::BotApi;
use self::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup};
use tokio::sync::Mutex;

use serde::Deserialize;

use crate::config::{ControllerConfig, Settings};
use crate::controller::Output;
use crate::media;
use crate::message::ContentBlock;

use self::formatting::{format_logs_for_telegram, strip_bot_mention};

pub use self::formatting::is_public_command;
use self::output::TelegramOutput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-chat agent state, tracking the active provider and model
/// so within-provider model switching works.
struct ChatAgent {
    agent: crate::agent::Agent,
    provider_name: String,
    model: String,
}

/// Per-chat entry with its own lock and quick-response state.
///
/// This replaces the old global `Mutex<HashMap<i64, ChatAgent>>` design.
/// Each chat gets its own mutex so that:
/// 1. Different chats never block each other.
/// 2. When a chat's agent is locked, new messages get a quick response
///    (single LLM call, no tools) via `try_lock()` instead of blocking.
struct ChatEntry {
    /// The agent, behind its own mutex (locked only during agent.run()).
    /// `try_lock()` is the gate: if it fails, the agent is busy and we
    /// fall back to a quick response.
    agent: Mutex<ChatAgent>,
    /// Snapshot of conversation messages, updated before each agent run.
    /// Quick response reads this when the agent is busy.
    messages_snapshot: tokio::sync::RwLock<Vec<crate::message::Message>>,
    /// System prompt for quick response context.
    system_prompt: tokio::sync::RwLock<String>,
    /// Completion config for quick response LLM calls.
    config: tokio::sync::RwLock<crate::llm::CompletionConfig>,
    /// Whether this chat is a group/supergroup.
    /// Group chats get restricted tools (web_search + web_fetch only)
    /// with SSRF protection always enabled.
    is_group: bool,
}

/// Minimum interval between message edits (milliseconds).
const EDIT_INTERVAL_MS: u128 = 500;

/// Maximum message length for Telegram (UTF-8 characters).
const MAX_MESSAGE_LEN: usize = 4000;

// ---------------------------------------------------------------------------
// TelegramController
// ---------------------------------------------------------------------------

/// Telegram-specific config fields, deserialized from the controller's
/// opaque JSON blob.
///
/// ```json
/// {
///   "type": "telegram",
///   "bot_token": "literal-token",
///   "allowed_chat_ids": [123456789]
/// }
/// ```
///
/// Or with a secret reference (resolved before this struct sees it):
/// ```json
/// {
///   "type": "telegram",
///   "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
///   "allowed_chat_ids": [123456789]
/// }
/// ```
#[derive(Debug, Deserialize)]
struct TelegramControllerConfig {
    /// Bot API token (already resolved from secret reference by the config loader).
    bot_token: String,
    /// Chat IDs allowed to interact.  Empty or absent = allow all.
    ///
    /// Accepts both numbers and strings (strings are parsed to i64).
    /// This is necessary because secret-resolved values become JSON
    /// strings — `{ "resolver": "insecure_env", "name": "MY_CHAT_ID" }`
    /// resolves to `"123456"` (a string), not `123456` (a number).
    #[serde(default, deserialize_with = "deserialize_chat_ids")]
    allowed_chat_ids: Vec<i64>,
    /// Explicitly acknowledge that the bot accepts messages from any chat.
    /// Required when `allowed_chat_ids` is empty, to prevent accidental
    /// open access from config errors.
    #[serde(default)]
    allow_all_chats: bool,
}

/// Deserialize chat IDs from a mix of numbers and strings.
///
/// Handles:
/// - `[123456789]` — JSON numbers
/// - `["123456789"]` — JSON strings (from resolved secrets)
/// - `[123, "456"]` — mixed
fn deserialize_chat_ids<'de, D>(deserializer: D) -> std::result::Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values: Vec<serde_json::Value> = Vec::deserialize(deserializer)?;
    let mut ids = Vec::new();
    for val in values {
        match val {
            serde_json::Value::Number(n) => {
                ids.push(n.as_i64().ok_or_else(|| {
                    serde::de::Error::custom(format!("chat ID {n} is not a valid i64"))
                })?);
            }
            serde_json::Value::String(s) => {
                ids.push(s.parse::<i64>().map_err(|_| {
                    serde::de::Error::custom(format!("chat ID '{s}' is not a valid number"))
                })?);
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "expected number or string for chat ID, got {other}"
                )));
            }
        }
    }
    Ok(ids)
}

/// Telegram bot controller.
pub struct TelegramController {
    /// Bot API token.  Uses `Credential` for zeroize-on-drop.
    bot_token: crate::auth::Credential,
    allowed_chat_ids: Vec<i64>,
}

impl TelegramController {
    /// Create from a ControllerConfig by parsing the opaque JSON blob.
    ///
    /// Returns `None` if the type doesn't match or if required fields
    /// (bot_token) are missing.
    pub fn from_config(config: &ControllerConfig) -> Option<Self> {
        if config.controller_type != "telegram" {
            return None;
        }

        let tg_config: TelegramControllerConfig =
            match serde_json::from_value(config.config.clone()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to parse telegram controller config — is bot_token set?"
                    );
                    return None;
                }
            };

        if tg_config.allowed_chat_ids.is_empty() && !tg_config.allow_all_chats {
            tracing::error!(
                "Telegram controller has no allowed_chat_ids and allow_all_chats is not set. \
                 Either add chat IDs to allowed_chat_ids or set \"allow_all_chats\": true \
                 to explicitly allow messages from any chat."
            );
            return None;
        }

        if tg_config.allowed_chat_ids.is_empty() {
            tracing::warn!(
                "Telegram bot will accept messages from ANY chat (allow_all_chats is set)"
            );
        }

        Some(Self {
            bot_token: crate::auth::Credential::new(tg_config.bot_token),
            allowed_chat_ids: tg_config.allowed_chat_ids,
        })
    }
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

    async fn run(&self, settings: &Settings) -> crate::Result<()> {
        eprintln!(
            "Dyson v{} — running as Telegram bot",
            env!("CARGO_PKG_VERSION")
        );

        let bot = BotApi::new(self.bot_token.expose());
        let allowed_ids = self.allowed_chat_ids.clone();
        let mut current_settings = settings.clone();
        let controller_prompt = self.system_prompt().map(|s| s.to_string());

        let (config_path, mut reloader) = super::create_hot_reloader(settings);

        let agents: Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));

        let chat_store: Arc<dyn crate::chat_history::ChatHistory> = {
            let store = crate::chat_history::create_chat_history(&settings.chat_history)?;
            Arc::from(store)
        };

        let transcriber = media::audio::create_transcriber(settings.transcriber.as_ref());

        let mut offset: i64 = 0;
        let mut consecutive_failures: u64 = 0;
        let mut backoff_secs: u64 = 1;

        loop {
            if let Ok((true, new_settings)) = reloader.check().await {
                if let Some(s) = new_settings {
                    current_settings = s;
                    current_settings.dangerous_no_sandbox = settings.dangerous_no_sandbox;
                }
                rebuild_agents_on_reload(
                    &agents,
                    &current_settings,
                    controller_prompt.as_deref(),
                    &chat_store,
                )
                .await;
            }

            // Poll for updates with a timeout, racing against Ctrl-C.
            let updates = tokio::select! {
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
            };

            for update in &updates {
                offset = update.update_id + 1;

                if let Some(cb) = &update.callback_query {
                    let cb_chat = cb.message.as_ref().map(|m| &m.chat);
                    let cb_chat_id = cb_chat.map(|c| ChatId(c.id));
                    let cb_is_group = cb_chat.is_some_and(|c| c.is_group());
                    let cb_data = cb.data.clone().unwrap_or_default();
                    let _ = bot.answer_callback_query(&cb.id).await;

                    if let Some(chat_id) = cb_chat_id {
                        if !cb_is_group
                            && !allowed_ids.is_empty()
                            && !allowed_ids.contains(&chat_id.0)
                        {
                            continue;
                        }
                        handle_callback_query(
                            &bot,
                            &cb_data,
                            chat_id,
                            &agents,
                            &current_settings,
                            controller_prompt.as_deref(),
                            config_path.as_deref(),
                            &chat_store,
                        )
                        .await;
                    }
                    continue;
                }

                let msg = match &update.message {
                    Some(m) => m.clone(),
                    None => continue,
                };

                let text = msg
                    .text
                    .as_deref()
                    .or(msg.caption.as_deref())
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_string());

                let has_media = msg.photo.is_some()
                    || msg.voice.is_some()
                    || msg.document.as_ref().is_some_and(|d| {
                        d.mime_type
                            .as_ref()
                            .is_some_and(|m| m.starts_with("image/"))
                    });

                if text.is_none() && !has_media {
                    continue;
                }

                let text = strip_bot_mention(&text.unwrap_or_default());
                let chat_id = ChatId(msg.chat.id);

                if is_public_command(&text) {
                    let _ = bot.send_message(chat_id, &chat_id.0.to_string()).await;
                    continue;
                }

                let is_group = msg.chat.is_group();

                // Group chats are always allowed — they run as public agents
                // with restricted tools, so they're safe without whitelisting.
                // Private chats require explicit allowed_chat_ids.
                if !is_group && !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id.0) {
                    tracing::warn!(chat_id = chat_id.0, "unauthorized private chat — ignoring");
                    continue;
                }

                if let Some(handled) = handle_instant_command(
                    &bot, &text, chat_id, &current_settings,
                ).await
                    && handled
                {
                    continue;
                }

                if text == "/clear" || text == "/compact" || text.starts_with("/model ") {
                    let entry = match get_or_create_entry(
                        &agents,
                        chat_id.0,
                        is_group,
                        &current_settings,
                        controller_prompt.as_deref(),
                        &chat_store,
                    )
                    .await
                    {
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
                        controller_prompt.as_deref(),
                        config_path.as_deref(),
                        &*chat_store,
                    )
                    .await;
                    continue;
                }

                tracing::info!(chat_id = chat_id.0, is_group, "telegram message received");

                let entry = match get_or_create_entry(
                    &agents,
                    chat_id.0,
                    is_group,
                    &current_settings,
                    controller_prompt.as_deref(),
                    &chat_store,
                )
                .await
                {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = bot.send_message(chat_id, &format!("Error: {e}")).await;
                        continue;
                    }
                };

                let bot_clone = bot.clone();
                let settings_clone = current_settings.clone();
                let store_clone = chat_store.clone();
                let transcriber_clone = transcriber.clone();
                tokio::spawn(run_agent_for_message(
                    bot_clone,
                    chat_id,
                    msg,
                    text,
                    entry,
                    settings_clone,
                    store_clone,
                    transcriber_clone,
                ));
            }
        }

        Ok(())
    }
}

/// Rebuild all per-chat agents after a config/workspace reload, preserving
/// each chat's provider/model selection and conversation history.
async fn rebuild_agents_on_reload(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    settings: &Settings,
    controller_prompt: Option<&str>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
) {
    let mut agents_map = agents.write().await;
    let old_agents: Vec<(i64, Arc<ChatEntry>)> = agents_map.drain().collect();
    for (chat_id, entry) in old_agents {
        let ca = entry.agent.lock().await;
        let provider_name = ca.provider_name.clone();
        let model = ca.model.clone();
        let messages = ca.agent.messages().to_vec();
        let is_group = entry.is_group;
        drop(ca);

        // Public agents rebuild from scratch; private agents preserve provider/model.
        let agent_result = if is_group {
            super::build_agent(settings, controller_prompt, true).await.map(|mut a| {
                a.set_messages(messages.clone());
                a
            })
        } else {
            super::build_agent_with_provider(
                settings,
                &provider_name,
                Some(&model),
                controller_prompt,
                messages.clone(),
            )
            .await
        };

        match agent_result {
            Ok(mut new_agent) => {
                new_agent.set_chat_history(
                    Arc::clone(chat_store),
                    chat_id.to_string(),
                );
                let sys_prompt = new_agent.system_prompt().to_string();
                let cfg = new_agent.config().clone();
                agents_map.insert(
                    chat_id,
                    Arc::new(ChatEntry {
                        agent: Mutex::new(ChatAgent {
                            agent: new_agent,
                            provider_name,
                            model,
                        }),
                        messages_snapshot: tokio::sync::RwLock::new(messages),
                        system_prompt: tokio::sync::RwLock::new(sys_prompt),
                        config: tokio::sync::RwLock::new(cfg),
                        is_group,
                    }),
                );
            }
            Err(e) => {
                tracing::warn!(
                    chat_id,
                    provider = provider_name,
                    model,
                    is_group,
                    error = %e,
                    "could not rebuild agent after reload — dropping",
                );
            }
        }
    }
    drop(agents_map);
    tracing::info!("config/workspace reloaded — agents rebuilt");
}

/// Handle a callback query (inline keyboard button press) for model switching.
#[allow(clippy::too_many_arguments)]
async fn handle_callback_query(
    bot: &BotApi,
    cb_data: &str,
    chat_id: ChatId,
    agents: &Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>>,
    settings: &Settings,
    controller_prompt: Option<&str>,
    config_path: Option<&std::path::Path>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
) {
    let Some(rest) = cb_data.strip_prefix("model:") else {
        return;
    };
    let Some((provider, model)) = rest.split_once(':') else {
        return;
    };

    let (existing_messages, is_group) = {
        let agents_map = agents.read().await;
        if let Some(entry) = agents_map.get(&chat_id.0) {
            let ca = entry.agent.lock().await;
            (ca.agent.messages().to_vec(), entry.is_group)
        } else {
            (Vec::new(), false)
        }
    };

    match super::build_agent_with_provider(
        settings,
        provider,
        Some(model),
        controller_prompt,
        existing_messages.clone(),
    )
    .await
    {
        Ok(mut new_agent) => {
            new_agent.set_chat_history(
                Arc::clone(chat_store),
                chat_id.0.to_string(),
            );
            let pc = &settings.providers[provider];
            let reply = format!(
                "Switched to '{}' — {:?} ({})",
                provider, pc.provider_type, model,
            );
            let sys_prompt = new_agent.system_prompt().to_string();
            let cfg = new_agent.config().clone();
            agents.write().await.insert(
                chat_id.0,
                Arc::new(ChatEntry {
                    agent: Mutex::new(ChatAgent {
                        agent: new_agent,
                        provider_name: provider.to_string(),
                        model: model.to_string(),
                    }),
                    messages_snapshot: tokio::sync::RwLock::new(existing_messages),
                    system_prompt: tokio::sync::RwLock::new(sys_prompt),
                    config: tokio::sync::RwLock::new(cfg),
                    is_group,
                }),
            );
            if let Some(cp) = config_path {
                crate::config::loader::persist_model_selection(cp, provider, model);
            }
            let _ = bot.send_message(chat_id, &reply).await;
        }
        Err(e) => {
            let _ = bot.send_message(chat_id, &format!("Switch error: {e}")).await;
        }
    }
}

/// Handle per-chat commands (/clear, /compact, /model) that need the agent lock.
#[allow(clippy::too_many_arguments)]
async fn handle_per_chat_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    entry: &Arc<ChatEntry>,
    settings: &Settings,
    controller_prompt: Option<&str>,
    config_path: Option<&std::path::Path>,
    chat_store: &dyn crate::chat_history::ChatHistory,
) {
    let mut ca = entry.agent.lock().await;
    let mut output = TelegramOutput::new(bot.clone(), chat_id, true);
    let ChatAgent {
        ref mut agent,
        ref mut provider_name,
        ref mut model,
    } = *ca;
    let result = super::execute_command(
        text,
        agent,
        &mut output,
        settings,
        provider_name,
        model,
        config_path,
        controller_prompt,
    )
    .await;

    match result {
        super::CommandResult::Cleared => {
            *entry.messages_snapshot.write().await = Vec::new();
            let _ = chat_store.rotate(&chat_id.0.to_string());
            let _ = bot.send_message(chat_id, "Context cleared.").await;
            tracing::info!(chat_id = chat_id.0, "conversation rotated and cleared");
        }
        super::CommandResult::Compacted => {
            let chat_key = chat_id.0.to_string();
            let msgs = ca.agent.messages().to_vec();
            *entry.messages_snapshot.write().await = msgs.clone();
            let _ = chat_store.save(&chat_key, &msgs);
            let _ = bot.send_message(chat_id, "Context compacted.").await;
            tracing::info!(chat_id = chat_id.0, "conversation compacted");
        }
        super::CommandResult::CompactError(e) => {
            let _ = bot.send_message(chat_id, &format!("Compaction failed: {e}")).await;
        }
        super::CommandResult::ModelSwitched {
            provider_name,
            provider_type,
            model,
        } => {
            *entry.system_prompt.write().await =
                ca.agent.system_prompt().to_string();
            *entry.config.write().await = ca.agent.config().clone();
            let _ = bot
                .send_message(
                    chat_id,
                    &format!("Switched to '{provider_name}' — {provider_type} ({model})"),
                )
                .await;
        }
        super::CommandResult::ModelSwitchError(e) => {
            let _ = bot.send_message(chat_id, &format!("Switch error: {e}")).await;
        }
        super::CommandResult::ModelParseError(e) => {
            let _ = bot.send_message(chat_id, &e).await;
        }
        super::CommandResult::ModelUsage => {
            let _ = bot
                .send_message(
                    chat_id,
                    "Usage: /model <provider> [model]  or  /model <model>",
                )
                .await;
        }
        _ => {}
    }
}

/// Run the agent for a message in a background task, with quick-response fallback.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_message(
    bot: BotApi,
    chat_id: ChatId,
    msg: types::Message,
    text: String,
    entry: Arc<ChatEntry>,
    settings: Settings,
    chat_store: Arc<dyn crate::chat_history::ChatHistory>,
    transcriber: Arc<dyn media::audio::Transcriber>,
) {
    let chat_key = chat_id.0.to_string();

    let content_blocks = match extract_content(&bot, &msg, &text, &transcriber).await {
        Ok(blocks) => blocks,
        Err(e) => {
            tracing::error!(error = %e, "failed to extract media content");
            let _ = bot.send_message(chat_id, &format!("Media error: {e}")).await;
            return;
        }
    };

    if content_blocks.is_empty() {
        return;
    }

    // try_lock() is the gate: if the agent is busy, fall back to a quick response.
    let mut ca = match entry.agent.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::info!(chat_id = chat_id.0, "agent busy — using quick response");
            send_quick_response(&bot, chat_id, &text, &entry, &settings).await;
            return;
        }
    };

    // Update snapshot so quick responses see latest context.
    *entry.messages_snapshot.write().await = ca.agent.messages().to_vec();

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let has_non_text = content_blocks
        .iter()
        .any(|b| !matches!(b, ContentBlock::Text { .. }));
    let result = if has_non_text {
        ca.agent.run_with_blocks(content_blocks, &mut output).await
    } else {
        ca.agent.run(&text, &mut output).await
    };

    if let Err(e) = result {
        tracing::error!(error = %e, "agent run failed");
        let _ = output.error(&e);
    }

    let msgs = ca.agent.messages().to_vec();
    *entry.messages_snapshot.write().await = msgs.clone();
    if let Err(e) = chat_store.save(&chat_key, &msgs) {
        tracing::error!(error = %e, "failed to save chat history");
    }
}

/// Send a quick response (no tools, fast) when the agent is busy.
async fn send_quick_response(
    bot: &BotApi,
    chat_id: ChatId,
    text: &str,
    entry: &ChatEntry,
    settings: &Settings,
) {
    let messages_snap = entry.messages_snapshot.read().await.clone();
    let sys_prompt = entry.system_prompt.read().await.clone();
    let config = entry.config.read().await.clone();

    let client = crate::llm::create_client(
        &settings.agent,
        None,
        settings.dangerous_no_sandbox,
    );

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let result = crate::agent::quick_response(
        client.as_ref(),
        &messages_snap,
        &sys_prompt,
        text,
        &config,
        &mut output,
    )
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "quick response failed");
        let _ = output.error(&e);
    }
}

/// Handle instant commands that don't need the agent lock.
///
/// Returns `Some(true)` if the command was handled (caller should `continue`),
/// `Some(false)` if recognized but not fully handled, and `None` if the text
/// is not an instant command.
async fn handle_instant_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    settings: &Settings,
) -> Option<bool> {
    if text == "/logs" || text.starts_with("/logs ") {
        let n: usize = text
            .strip_prefix("/logs")
            .unwrap()
            .trim()
            .parse()
            .unwrap_or(20);
        let result = tokio::task::spawn_blocking(move || super::read_log_tail(n)).await;
        let reply = match result {
            Ok(Ok(lines)) => format_logs_for_telegram(&lines),
            Ok(Err(e)) => vec![format!("Logs error: {e}")],
            Err(e) => vec![format!("Logs error: {e}")],
        };
        for part in reply {
            let _ = bot.send_message_html(chat_id, &part).await;
        }
        return Some(true);
    }

    if text == "/memory" {
        let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
        return Some(true);
    }
    if let Some(note) = text.strip_prefix("/memory ") {
        let note = note.trim();
        if note.is_empty() {
            let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
            return Some(true);
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
        return Some(true);
    }

    if text == "/models" {
        handle_models_command(bot, chat_id, settings).await;
        return Some(true);
    }

    if text == "/model" {
        let _ = bot
            .send_message(
                chat_id,
                "Usage: /model <provider> [model]  or  /model <model>",
            )
            .await;
        return Some(true);
    }

    None
}

/// Handle the /models command — show an inline keyboard with all providers/models.
async fn handle_models_command(bot: &BotApi, chat_id: ChatId, settings: &Settings) {
    if settings.providers.is_empty() {
        let _ = bot.send_message(chat_id, "No providers configured.").await;
        return;
    }

    let current_provider =
        super::active_provider_name(settings).unwrap_or_default();
    let current_model = &settings.agent.model;
    let providers: Vec<super::ProviderInfo> = super::list_providers(settings)
        .into_iter()
        .map(|(name, pc)| super::ProviderInfo {
            name: name.to_string(),
            provider_type: format!("{:?}", pc.provider_type),
            models: pc
                .models
                .iter()
                .map(|m| super::ModelInfo {
                    name: m.clone(),
                    active: name == current_provider.as_str() && m == current_model,
                })
                .collect(),
        })
        .collect();
    let keyboard = build_model_keyboard(&providers);
    let _ = bot.send_message_with_keyboard(chat_id, "Select a model:", &keyboard).await;
}

/// Get or create a per-chat entry, restoring persisted history if available.
///
/// Takes a read lock on the map first (fast path).  Only upgrades to a write
/// lock if the entry doesn't exist yet.  This means existing chats never
/// contend on the map lock — only the first message in a new chat takes the
/// write lock briefly.
async fn get_or_create_entry(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    chat_id: i64,
    is_group: bool,
    settings: &Settings,
    controller_prompt: Option<&str>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
) -> crate::Result<Arc<ChatEntry>> {
    // Fast path: entry already exists.
    {
        let map = agents.read().await;
        if let Some(entry) = map.get(&chat_id) {
            return Ok(Arc::clone(entry));
        }
    }

    // Slow path: create a new agent for this chat.
    // Public (group) agents get restricted tools; private agents get everything.
    let mut agent =
        crate::controller::build_agent(settings, controller_prompt, is_group).await?;

    let chat_key = chat_id.to_string();

    // Attach chat history so compaction can rotate pre-compaction snapshots.
    agent.set_chat_history(Arc::clone(chat_store), chat_key.clone());

    let mut restored_messages = Vec::new();
    if let Ok(messages) = chat_store.load(&chat_key)
        && !messages.is_empty()
    {
        tracing::info!(
            chat_id,
            messages = messages.len(),
            "restored chat history"
        );
        agent.set_messages(messages.clone());
        restored_messages = messages;
    }

    let provider_name =
        super::active_provider_name(settings).unwrap_or_default();
    let model = settings.agent.model.clone();
    let sys_prompt = agent.system_prompt().to_string();
    let config = agent.config().clone();

    let entry = Arc::new(ChatEntry {
        agent: Mutex::new(ChatAgent {
            agent,
            provider_name,
            model,
        }),
        messages_snapshot: tokio::sync::RwLock::new(restored_messages),
        system_prompt: tokio::sync::RwLock::new(sys_prompt),
        config: tokio::sync::RwLock::new(config),
        is_group,
    });

    // Insert under write lock.  Another task may have raced us, so use
    // entry API to avoid overwriting.
    let mut map = agents.write().await;
    let entry = Arc::clone(
        map.entry(chat_id).or_insert_with(|| Arc::clone(&entry)),
    );
    Ok(entry)
}

/// Build an inline keyboard listing all providers and models.
///
/// Each button shows the model name (with a check mark for the active one).
/// The callback data encodes `model:{provider}:{model}` so the handler
/// can switch to the selected model.
fn build_model_keyboard(providers: &[super::ProviderInfo]) -> InlineKeyboardMarkup {
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

/// Save a note to the workspace MEMORY.md file.
fn save_memory_note(settings: &Settings, note: &str) -> crate::Result<()> {
    let mut workspace = crate::workspace::create_workspace(&settings.workspace)?;

    let today = crate::workspace::OpenClawWorkspace::today_date();
    let entry = format!("\n- [{today}] {note}");

    workspace.append("MEMORY.md", &entry);
    workspace.save()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Media extraction helpers
// ---------------------------------------------------------------------------

/// Extract content blocks from a Telegram message.
///
/// Downloads and processes media (photos, voice notes, image documents)
/// into `ContentBlock`s.  This function is media-only — it does not know
/// about model capabilities.  If a model rejects the resulting blocks,
/// the caller handles cleanup.
async fn extract_content(
    bot: &BotApi,
    msg: &types::Message,
    text: &str,
    transcriber: &Arc<dyn media::audio::Transcriber>,
) -> crate::Result<Vec<ContentBlock>> {
    let mut blocks = Vec::new();

    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    // Photos: pick the largest resolution (last in the array).
    if let Some(photos) = &msg.photo
        && let Some(photo) = photos.last()
    {
        tracing::info!(
            file_id = photo.file_id.as_str(),
            width = photo.width,
            height = photo.height,
            "downloading photo from Telegram"
        );
        match bot.download_file(&photo.file_id).await {
            Ok(data) => match media::resolve(
                media::MediaInput::Image {
                    data,
                    mime_type: "image/jpeg".to_string(),
                },
                transcriber,
            )
            .await
            {
                Ok(media::ResolvedMedia::Images(imgs)) => blocks.extend(imgs),
                Ok(media::ResolvedMedia::Transcription(t)) => {
                    blocks.push(ContentBlock::Text { text: t });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to process photo");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Image could not be processed: {e}]"),
                    });
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "failed to download photo");
                blocks.push(ContentBlock::Text {
                    text: format!("[Failed to download photo: {e}]"),
                });
            }
        }
    }

    // Voice notes: transcribe via Whisper.
    if let Some(voice) = &msg.voice {
        tracing::info!(
            file_id = voice.file_id.as_str(),
            "downloading voice note from Telegram"
        );
        let mime = voice
            .mime_type
            .as_deref()
            .unwrap_or("audio/ogg")
            .to_string();

        match bot.download_file(&voice.file_id).await {
            Ok(data) => match media::resolve(
                media::MediaInput::Audio {
                    data,
                    mime_type: mime,
                },
                transcriber,
            )
            .await
            {
                Ok(media::ResolvedMedia::Transcription(t)) => {
                    blocks.push(ContentBlock::Text {
                        text: format!("[Voice transcription]: {t}"),
                    });
                }
                Ok(media::ResolvedMedia::Images(_)) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "failed to transcribe voice note");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Voice transcription failed: {e}]"),
                    });
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "failed to download voice note");
                blocks.push(ContentBlock::Text {
                    text: format!("[Failed to download voice note: {e}]"),
                });
            }
        }
    }

    // Documents with image MIME types (e.g. uncompressed photos).
    if let Some(doc) = &msg.document {
        let is_image = doc
            .mime_type
            .as_ref()
            .is_some_and(|m| m.starts_with("image/"));
        if is_image {
            tracing::info!(
                file_id = doc.file_id.as_str(),
                file_name = doc.file_name.as_deref().unwrap_or("unknown"),
                "downloading image document from Telegram"
            );
            let mime = doc
                .mime_type
                .as_deref()
                .unwrap_or("image/jpeg")
                .to_string();

            match bot.download_file(&doc.file_id).await {
                Ok(data) => match media::resolve(
                    media::MediaInput::Image {
                        data,
                        mime_type: mime,
                    },
                    transcriber,
                )
                .await
                {
                    Ok(media::ResolvedMedia::Images(imgs)) => blocks.extend(imgs),
                    Ok(media::ResolvedMedia::Transcription(t)) => {
                        blocks.push(ContentBlock::Text { text: t });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to process document image");
                        blocks.push(ContentBlock::Text {
                            text: format!("[Image could not be processed: {e}]"),
                        });
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "failed to download document");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Failed to download document: {e}]"),
                    });
                }
            }
        }
    }

    if blocks.is_empty() {
        blocks.push(ContentBlock::Text {
            text: "[Empty message received]".to_string(),
        });
    }

    Ok(blocks)
}

// TelegramOutput and formatting helpers are in submodules:
// - output.rs    — TelegramOutput struct and Output trait impl
// - formatting.rs — markdown-to-HTML conversion, message splitting, tests
