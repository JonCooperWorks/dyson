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

        // Fetch the bot's own identity so we can filter group messages
        // to only those that @mention or reply to the bot.
        let me = bot.get_me().await?;
        let bot_username = me.username.unwrap_or_default().to_lowercase();
        let bot_id = me.id;
        if bot_username.is_empty() {
            tracing::warn!("getMe returned no username — group mention filtering will not work");
        } else {
            tracing::info!(bot_username = bot_username.as_str(), "bot identity fetched");
        }

        let allowed_ids = self.allowed_chat_ids.clone();
        let mut current_settings = settings.clone();
        let controller_prompt = self.system_prompt().map(|s| s.to_string());

        let (config_path, mut reloader) = super::create_hot_reloader(settings);

        // Lazily-loaded client registry — one LLM client per provider,
        // shared across all agents and surviving provider switches.
        let mut registry = super::ClientRegistry::new(&current_settings, None);

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
                // Recreate the client registry so new API keys take effect.
                registry = super::ClientRegistry::new(&current_settings, None);
                rebuild_agents_on_reload(
                    &agents,
                    &current_settings,
                    controller_prompt.as_deref(),
                    &chat_store,
                    &mut registry,
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
                            config_path.as_deref(),
                            &mut registry,
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
                if !is_group && !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id.0) {
                    tracing::warn!(chat_id = chat_id.0, "unauthorized private chat — ignoring");
                    continue;
                }

                // In groups, only process messages directed at this bot to
                // avoid burning tokens on every group message.  Directed means:
                //   - a /command (Telegram delivers these even with privacy mode)
                //   - an @mention of this bot in the message text
                //   - a reply to one of this bot's own messages
                if is_group {
                    let is_directed = text.starts_with('/')
                        || is_bot_mentioned(&msg, &bot_username)
                        || is_reply_to_bot(&msg, bot_id);
                    if !is_directed {
                        continue;
                    }

                    // In group chats, only operators (users whose IDs are
                    // in allowed_chat_ids) may use / commands.  Non-operators
                    // can still talk to the bot via @mention or reply.
                    if text.starts_with('/') && !is_public_command(&text) {
                        let sender_id = msg.from.as_ref().map(|u| u.id);
                        if !is_operator(sender_id, &allowed_ids) {
                            tracing::info!(
                                chat_id = chat_id.0,
                                sender_id = ?sender_id,
                                command = %text,
                                "non-operator command in group — ignoring",
                            );
                            continue;
                        }
                    }
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
                        &mut registry,
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
                        config_path.as_deref(),
                        &*chat_store,
                        &mut registry,
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
                    &mut registry,
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
                let store_clone = chat_store.clone();
                let transcriber_clone = transcriber.clone();
                let client_for_task = registry.get_default();
                tokio::spawn(run_agent_for_message(
                    bot_clone,
                    chat_id,
                    msg,
                    text,
                    entry,
                    store_clone,
                    transcriber_clone,
                    client_for_task,
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
    registry: &mut super::ClientRegistry,
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

        // Both public and private agents are rebuilt from scratch on config
        // reload (cheap — allocation is fine here).  Private agents with a
        // non-default provider/model get a swap_client after building.
        let default_client = registry.get_default();
        let mode = if is_group {
            super::AgentMode::Public
        } else {
            super::AgentMode::Private
        };
        let agent_result = super::build_agent(settings, controller_prompt, mode, default_client, registry)
            .await
            .map(|mut a| {
                a.set_messages(messages.clone());
                // If this private agent was using a non-default provider, swap
                // to the correct client from the registry.
                if !is_group {
                    if let Some(pc) = settings.providers.get(&provider_name) {
                        if let Ok(handle) = registry.get(&provider_name) {
                            a.swap_client(handle, &model, &pc.provider_type);
                        }
                    }
                }
                a
            });

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
    config_path: Option<&std::path::Path>,
    registry: &mut super::ClientRegistry,
) {
    let Some(rest) = cb_data.strip_prefix("model:") else {
        return;
    };
    let Some((provider, model)) = rest.split_once(':') else {
        return;
    };

    let pc = match settings.providers.get(provider) {
        Some(pc) => pc,
        None => {
            let _ = bot.send_message(chat_id, &format!("Unknown provider '{provider}'")).await;
            return;
        }
    };

    let handle = match registry.get(provider) {
        Ok(h) => h,
        Err(e) => {
            let _ = bot.send_message(chat_id, &format!("Switch error: {e}")).await;
            return;
        }
    };

    // Hot-swap the client on the existing agent — no rebuild needed.
    let agents_map = agents.read().await;
    if let Some(entry) = agents_map.get(&chat_id.0) {
        let mut ca = entry.agent.lock().await;
        ca.agent.swap_client(handle, model, &pc.provider_type);
        ca.provider_name = provider.to_string();
        ca.model = model.to_string();
        // Update cached state for quick responses.
        *entry.system_prompt.write().await = ca.agent.system_prompt().to_string();
        *entry.config.write().await = ca.agent.config().clone();
    }
    drop(agents_map);

    if let Some(cp) = config_path {
        crate::config::loader::persist_model_selection(cp, provider, model);
    }
    let reply = format!(
        "Switched to '{}' — {:?} ({})",
        provider, pc.provider_type, model,
    );
    let _ = bot.send_message(chat_id, &reply).await;
}

/// Handle per-chat commands (/clear, /compact, /model) that need the agent lock.
#[allow(clippy::too_many_arguments)]
async fn handle_per_chat_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    entry: &Arc<ChatEntry>,
    settings: &Settings,
    config_path: Option<&std::path::Path>,
    chat_store: &dyn crate::chat_history::ChatHistory,
    registry: &mut super::ClientRegistry,
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
        registry,
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
    chat_store: Arc<dyn crate::chat_history::ChatHistory>,
    transcriber: Arc<dyn media::audio::Transcriber>,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
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
            send_quick_response(&bot, chat_id, &text, &entry, &client).await;
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
///
/// Uses the shared client handle — no new LLM client is created.
async fn send_quick_response(
    bot: &BotApi,
    chat_id: ChatId,
    text: &str,
    entry: &ChatEntry,
    client: &crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
) {
    let messages_snap = entry.messages_snapshot.read().await.clone();
    let sys_prompt = entry.system_prompt.read().await.clone();
    let config = entry.config.read().await.clone();

    let llm_client = match client.access() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::warn!(error = %e, "quick response rate-limited");
            return;
        }
    };

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let result = crate::agent::quick_response(
        &**llm_client,
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

/// Returns true if the message text mentions the given bot username.
///
/// Uses a case-insensitive search with a word-boundary check to avoid
/// false positives from longer usernames that happen to contain the bot's
/// name as a substring.
fn is_bot_mentioned(msg: &types::Message, bot_username: &str) -> bool {
    if bot_username.is_empty() {
        return false;
    }
    let text = msg
        .text
        .as_deref()
        .or(msg.caption.as_deref())
        .unwrap_or_default();
    let lower = text.to_lowercase();
    let target = format!("@{bot_username}");
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(&target) {
        let after = search_from + rel + target.len();
        // Valid Telegram username chars: [a-zA-Z0-9_]
        let at_boundary = after >= lower.len()
            || !lower.as_bytes()[after].is_ascii_alphanumeric()
                && lower.as_bytes()[after] != b'_';
        if at_boundary {
            return true;
        }
        search_from += rel + 1;
    }
    false
}

/// Returns true if the message is a reply to a message sent by the given bot.
fn is_reply_to_bot(msg: &types::Message, bot_id: i64) -> bool {
    msg.reply_to_message
        .as_ref()
        .is_some_and(|reply| reply.from.as_ref().is_some_and(|from| from.id == bot_id))
}

/// Returns true if the sender is an operator (their user ID is in the
/// allowed-chat-ids list).  In Telegram, private-chat IDs equal user IDs,
/// so `allowed_chat_ids` doubles as the operator allowlist.
pub fn is_operator(sender_id: Option<i64>, allowed_ids: &[i64]) -> bool {
    sender_id.is_some_and(|id| allowed_ids.contains(&id))
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
    registry: &mut super::ClientRegistry,
) -> crate::Result<Arc<ChatEntry>> {
    // Fast path: entry already exists.
    {
        let map = agents.read().await;
        if let Some(entry) = map.get(&chat_id) {
            return Ok(Arc::clone(entry));
        }
    }

    // Slow path: create a new agent for this chat.
    let mode = if is_group {
        crate::controller::AgentMode::Public
    } else {
        crate::controller::AgentMode::Private
    };
    let client = registry.get_default();
    let mut agent =
        crate::controller::build_agent(settings, controller_prompt, mode, client, registry).await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::Controller;
    use types::{Chat, ChatType, Message, User};

    fn make_msg(text: &str, chat_type: ChatType) -> Message {
        Message {
            message_id: 1,
            chat: Chat {
                id: 100,
                chat_type,
            },
            from: None,
            text: Some(text.to_string()),
            caption: None,
            entities: None,
            reply_to_message: None,
            photo: None,
            voice: None,
            document: None,
        }
    }

    fn make_group_msg(text: &str) -> Message {
        make_msg(text, ChatType::Supergroup)
    }

    #[test]
    fn mention_exact_match() {
        let msg = make_group_msg("@dysonbot hello");
        assert!(is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn mention_case_insensitive() {
        let msg = make_group_msg("@DysonBot hello");
        assert!(is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn mention_mid_text() {
        let msg = make_group_msg("hey @dysonbot what's up");
        assert!(is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn mention_end_of_text() {
        let msg = make_group_msg("hey @dysonbot");
        assert!(is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn mention_not_substring_of_longer_name() {
        let msg = make_group_msg("@dysonbot_extra hello");
        assert!(!is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn mention_empty_username() {
        let msg = make_group_msg("@dysonbot hello");
        assert!(!is_bot_mentioned(&msg, ""));
    }

    #[test]
    fn mention_no_mention() {
        let msg = make_group_msg("just chatting");
        assert!(!is_bot_mentioned(&msg, "dysonbot"));
    }

    #[test]
    fn reply_to_bot_detected() {
        let mut msg = make_group_msg("thanks");
        msg.reply_to_message = Some(Box::new(Message {
            message_id: 0,
            chat: msg.chat.clone(),
            from: Some(User {
                id: 42,
                is_bot: true,
                username: Some("dysonbot".to_string()),
            }),
            text: Some("here's the answer".to_string()),
            caption: None,
            entities: None,
            reply_to_message: None,
            photo: None,
            voice: None,
            document: None,
        }));
        assert!(is_reply_to_bot(&msg, 42));
    }

    #[test]
    fn reply_to_human_not_detected() {
        let mut msg = make_group_msg("thanks");
        msg.reply_to_message = Some(Box::new(Message {
            message_id: 0,
            chat: msg.chat.clone(),
            from: Some(User {
                id: 99,
                is_bot: false,
                username: Some("alice".to_string()),
            }),
            text: Some("some message".to_string()),
            caption: None,
            entities: None,
            reply_to_message: None,
            photo: None,
            voice: None,
            document: None,
        }));
        assert!(!is_reply_to_bot(&msg, 42));
    }

    #[test]
    fn no_reply_not_detected() {
        let msg = make_group_msg("hello");
        assert!(!is_reply_to_bot(&msg, 42));
    }

    // -------------------------------------------------------------------
    // Chat::is_group — mode-selection input for AgentMode::Public
    // -------------------------------------------------------------------

    #[test]
    fn is_group_for_group_chat() {
        let chat = Chat { id: 1, chat_type: ChatType::Group };
        assert!(chat.is_group(), "Group chats should be identified as groups");
    }

    #[test]
    fn is_group_for_supergroup_chat() {
        let chat = Chat { id: 1, chat_type: ChatType::Supergroup };
        assert!(chat.is_group(), "Supergroup chats should be identified as groups");
    }

    #[test]
    fn is_group_false_for_private_chat() {
        let chat = Chat { id: 1, chat_type: ChatType::Private };
        assert!(!chat.is_group(), "Private chats should not be identified as groups");
    }

    #[test]
    fn is_group_false_for_channel() {
        let chat = Chat { id: 1, chat_type: ChatType::Channel };
        assert!(!chat.is_group(), "Channels should not be identified as groups");
    }

    // -------------------------------------------------------------------
    // Group chat mode → AgentMode::Public mapping
    // -------------------------------------------------------------------

    #[test]
    fn group_chat_maps_to_public_mode() {
        // Replicate the mode selection logic from get_or_create_entry.
        let is_group = true;
        let mode = if is_group {
            crate::controller::AgentMode::Public
        } else {
            crate::controller::AgentMode::Private
        };
        assert_eq!(mode, crate::controller::AgentMode::Public);
    }

    #[test]
    fn private_chat_maps_to_private_mode() {
        let is_group = false;
        let mode = if is_group {
            crate::controller::AgentMode::Public
        } else {
            crate::controller::AgentMode::Private
        };
        assert_eq!(mode, crate::controller::AgentMode::Private);
    }

    // -------------------------------------------------------------------
    // Access control: group chats bypass allowed_chat_ids
    // -------------------------------------------------------------------

    /// Replicate the authorization check from the message loop.
    /// Returns true if the message should be REJECTED.
    fn should_reject(is_group: bool, allowed_ids: &[i64], chat_id: i64) -> bool {
        !is_group && !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id)
    }

    #[test]
    fn group_chat_bypasses_allowed_ids() {
        // Group chats are never rejected, even if not in allowed_ids.
        assert!(!should_reject(true, &[111, 222], 999));
    }

    #[test]
    fn group_chat_allowed_with_empty_allowlist() {
        assert!(!should_reject(true, &[], 999));
    }

    #[test]
    fn private_chat_rejected_when_not_in_allowlist() {
        assert!(should_reject(false, &[111, 222], 999));
    }

    #[test]
    fn private_chat_allowed_when_in_allowlist() {
        assert!(!should_reject(false, &[111, 222], 222));
    }

    #[test]
    fn private_chat_allowed_with_empty_allowlist() {
        // Empty allowlist = allow all private chats (guarded by allow_all_chats at init).
        assert!(!should_reject(false, &[], 999));
    }

    // -------------------------------------------------------------------
    // Operator-only commands in group chats
    // -------------------------------------------------------------------

    /// Replicate the operator gate from the message loop.
    /// In group chats, only operators (users whose IDs are in
    /// `allowed_chat_ids`) may use / commands.
    /// Returns true if the command should be REJECTED.
    fn should_reject_group_command(
        is_group: bool,
        text: &str,
        sender_id: Option<i64>,
        allowed_ids: &[i64],
    ) -> bool {
        is_group
            && text.starts_with('/')
            && !is_public_command(text)
            && !is_operator(sender_id, allowed_ids)
    }

    #[test]
    fn group_command_from_non_operator_rejected() {
        // A random user (id=999) NOT in the allowed list sends /clear in a group.
        assert!(should_reject_group_command(
            true,
            "/clear",
            Some(999),
            &[111, 222],
        ));
    }

    #[test]
    fn group_command_from_operator_allowed() {
        // An operator (id=111) in the allowed list sends /clear in a group.
        assert!(!should_reject_group_command(
            true,
            "/clear",
            Some(111),
            &[111, 222],
        ));
    }

    #[test]
    fn group_command_from_unknown_sender_rejected() {
        // No `from` field at all — should be rejected.
        assert!(should_reject_group_command(
            true,
            "/logs",
            None,
            &[111, 222],
        ));
    }

    #[test]
    fn group_plain_message_from_non_operator_allowed() {
        // Non-operator sends a regular message (not a command) in a group.
        assert!(!should_reject_group_command(
            true,
            "hello bot",
            Some(999),
            &[111, 222],
        ));
    }

    #[test]
    fn private_command_unaffected_by_operator_check() {
        // In private chats the operator gate does not apply.
        assert!(!should_reject_group_command(
            false,
            "/clear",
            Some(999),
            &[111, 222],
        ));
    }

    #[test]
    fn group_whoami_allowed_for_non_operator() {
        // /whoami is a public command, allowed for everyone even in groups.
        assert!(!should_reject_group_command(
            true,
            "/whoami",
            Some(999),
            &[111, 222],
        ));
    }

    #[test]
    fn group_command_all_commands_restricted_for_non_operator() {
        let commands = [
            "/logs", "/logs 50", "/memory", "/memory some note",
            "/clear", "/compact", "/model provider", "/models",
        ];
        for cmd in commands {
            assert!(
                should_reject_group_command(true, cmd, Some(999), &[111, 222]),
                "{cmd} should be rejected for non-operators in groups",
            );
        }
    }

    // -------------------------------------------------------------------
    // Group message direction filtering
    // -------------------------------------------------------------------

    /// Replicate the directed-message check for group chats.
    fn is_directed_group_msg(msg: &Message, bot_username: &str, bot_id: i64) -> bool {
        let text = msg.text.as_deref().unwrap_or_default();
        text.starts_with('/')
            || is_bot_mentioned(msg, bot_username)
            || is_reply_to_bot(msg, bot_id)
    }

    #[test]
    fn group_command_is_directed() {
        let msg = make_group_msg("/help");
        assert!(is_directed_group_msg(&msg, "dysonbot", 42));
    }

    #[test]
    fn group_mention_is_directed() {
        let msg = make_group_msg("hey @dysonbot what's the weather?");
        assert!(is_directed_group_msg(&msg, "dysonbot", 42));
    }

    #[test]
    fn group_reply_to_bot_is_directed() {
        let mut msg = make_group_msg("thanks");
        msg.reply_to_message = Some(Box::new(Message {
            message_id: 0,
            chat: msg.chat.clone(),
            from: Some(User {
                id: 42,
                is_bot: true,
                username: Some("dysonbot".to_string()),
            }),
            text: Some("previous answer".to_string()),
            caption: None,
            entities: None,
            reply_to_message: None,
            photo: None,
            voice: None,
            document: None,
        }));
        assert!(is_directed_group_msg(&msg, "dysonbot", 42));
    }

    #[test]
    fn group_undirected_message_not_directed() {
        let msg = make_group_msg("just chatting with friends");
        assert!(!is_directed_group_msg(&msg, "dysonbot", 42));
    }

    // -------------------------------------------------------------------
    // Public command (/whoami) — available to all chats
    // -------------------------------------------------------------------

    #[test]
    fn whoami_is_public_command() {
        assert!(formatting::is_public_command("/whoami"));
    }

    #[test]
    fn other_commands_are_not_public() {
        assert!(!formatting::is_public_command("/help"));
        assert!(!formatting::is_public_command("/clear"));
        assert!(!formatting::is_public_command("/model"));
        assert!(!formatting::is_public_command("whoami"));
    }

    // -------------------------------------------------------------------
    // Telegram controller prompt injected alongside identity
    // -------------------------------------------------------------------

    #[test]
    fn telegram_prompt_coexists_with_identity_in_public_agent() {
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("SOUL.md", "I speak like a pirate.")
            .with_file("IDENTITY.md", "I am Captain Bot.");

        let mut agent_settings = crate::config::AgentSettings::default();
        super::super::inject_workspace_identity(&ws, &mut agent_settings);

        // Append public-agent and Telegram-controller prompts the same
        // way build_public_agent and the Telegram controller would.
        agent_settings.system_prompt.push_str(
            "\n\nYou are a public-facing agent with limited tools.",
        );
        let telegram_prompt = "You are responding via Telegram. Keep these rules:\n\
             - Keep responses concise. Telegram messages have a 4096 character limit.";
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(telegram_prompt);

        let prompt = &agent_settings.system_prompt;
        // Identity content present.
        assert!(prompt.contains("## PERSONALITY"), "should contain PERSONALITY");
        assert!(prompt.contains("I speak like a pirate."), "should contain SOUL.md content");
        assert!(prompt.contains("## IDENTITY"), "should contain IDENTITY");
        assert!(prompt.contains("I am Captain Bot."), "should contain IDENTITY.md content");
        // Public-agent suffix present.
        assert!(prompt.contains("public-facing agent"), "should contain public suffix");
        // Telegram controller prompt present.
        assert!(prompt.contains("Telegram"), "should contain Telegram prompt");
        assert!(prompt.contains("4096"), "should mention character limit");
    }

    #[test]
    fn telegram_controller_has_system_prompt() {
        // Verify the TelegramController always provides a system prompt
        // that will be appended to both private and public agents.
        let ctrl = TelegramController {
            bot_token: crate::auth::Credential::new("test".into()),
            allowed_chat_ids: vec![],
        };
        let prompt = ctrl.system_prompt().expect("Telegram controller must provide a system prompt");
        assert!(prompt.contains("Telegram"), "should reference Telegram");
        assert!(prompt.contains("4096"), "should mention the message character limit");
    }
}
