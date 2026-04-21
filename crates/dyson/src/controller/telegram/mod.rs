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
//     │     ├── lock-free commands → respond instantly (no agent lock):
//     │     │     /logs, /models, /agents, /loop, /stop, /memory
//     │     ├── per-chat commands → acquire agent lock:
//     │     │     /clear, /compact, /model
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
pub mod feedback;
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

use self::formatting::{format_logs_for_telegram, strip_bot_mention};

pub use self::formatting::is_public_command;
use self::output::TelegramOutput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-chat agent state, tracking the active provider and model
/// so within-provider model switching works.
///
/// `agent` is `Option` so it can be temporarily extracted (`.take()`) to
/// release the mutex during long-running operations like `/compact`.
struct ChatAgent {
    agent: Option<crate::agent::Agent>,
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
    /// Group chats run as public agents with per-channel workspace
    /// (workspace memory + web tools, no filesystem/shell).
    is_group: bool,
    /// Maps Telegram message IDs (sent by the bot) → conversation turn index.
    /// Used to associate emoji reactions with the correct assistant response.
    /// In-memory only — rebuilt each session.  Cleared on `/clear`.
    message_id_map: tokio::sync::RwLock<HashMap<i32, usize>>,
    /// When this chat last received a message.  Used for LRU eviction.
    last_active: std::sync::atomic::AtomicI64,
    /// Bounds the number of in-flight tasks per chat so a single user
    /// flooding the bot cannot spawn unlimited background LLM calls.
    /// `try_acquire_owned` is non-blocking: if no permit is available
    /// the message is dropped with a log line rather than queueing.
    in_flight: Arc<tokio::sync::Semaphore>,
}

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
/// Per-filetype download size limits for Telegram file handling.
///
/// Prevents OOM from oversized files.  Limits are checked both against the
/// Telegram `file_size` metadata (early reject) and incrementally during
/// the streaming download.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DownloadLimits {
    /// Maximum bytes for image files (photos and image documents).
    image_max_bytes: u64,
    /// Maximum bytes for audio/voice files.
    audio_max_bytes: u64,
    /// Maximum bytes for other document types.
    document_max_bytes: u64,
    /// Maximum bytes for text-like document types inlined into the prompt.
    /// Kept small because these go straight into the model context.
    text_max_bytes: u64,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            image_max_bytes: 50 * 1024 * 1024,    // 50 MB
            audio_max_bytes: 50 * 1024 * 1024,     // 50 MB
            document_max_bytes: 200 * 1024 * 1024,  // 200 MB
            text_max_bytes: 1024 * 1024,           // 1 MiB
        }
    }
}

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
    /// Per-filetype download size limits.
    #[serde(default)]
    download_limits: DownloadLimits,
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
    download_limits: Arc<DownloadLimits>,
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
            download_limits: Arc::new(tg_config.download_limits),
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

    async fn run(
        &self,
        settings: &Settings,
        registry: &std::sync::Arc<super::ClientRegistry>,
    ) -> crate::Result<()> {
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
        let download_limits = Arc::clone(&self.download_limits);
        let mut current_settings = settings.clone();
        let controller_prompt = self.system_prompt().map(std::string::ToString::to_string);

        let (config_path, mut reloader) = super::create_hot_reloader(settings);

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

        loop {
            if let Ok((true, new_settings)) = reloader.check().await {
                if let Some(s) = new_settings {
                    current_settings = s;
                    current_settings.dangerous_no_sandbox = settings.dangerous_no_sandbox;
                }
                // Reload the client registry so new API keys take effect.
                registry.reload(&current_settings, None);
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
                            registry,
                            &bg_registry,
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
                    registry, &bg_registry, &agents, &chat_store,
                ).await
                    && handled
                {
                    continue;
                }

                if text == "/clear"
                    || text == "/compact"
                    || text.starts_with("/model ")
                {
                    let chat_cx = ChatContext {
                        settings: &current_settings,
                        controller_prompt: controller_prompt.as_deref(),
                        chat_store: &chat_store,
                        feedback_dir: &feedback_dir,
                        registry,
                    };
                    let entry = match get_or_create_entry(
                        &agents,
                        chat_id.0,
                        is_group,
                        &chat_cx,
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
                let entry = match get_or_create_entry(
                    &agents,
                    chat_id.0,
                    is_group,
                    &chat_cx,
                )
                .await
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
                    )
                    .await;
                    drop(permit);
                });
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
    feedback_dir: &std::path::Path,
    registry: &super::ClientRegistry,
) {
    let mut agents_map = agents.write().await;
    let old_agents: Vec<(i64, Arc<ChatEntry>)> = agents_map.drain().collect();
    for (chat_id, entry) in old_agents {
        let ca = entry.agent.lock().await;
        let provider_name = ca.provider_name.clone();
        let model = ca.model.clone();
        let messages = ca.agent.as_ref().expect("agent not available").messages().to_vec();
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
        let channel_id_str = chat_id.to_string();
        let ch = if is_group { Some(channel_id_str.as_str()) } else { None };
        let agent_result = super::build_agent(settings, controller_prompt, mode, default_client, registry, ch)
            .await
            .map(|mut a| {
                a.set_messages(messages.clone());
                // If this private agent was using a non-default provider, swap
                // to the correct client from the registry.
                if !is_group
                    && let Some(pc) = settings.providers.get(&provider_name)
                    && let Ok(handle) = registry.get(&provider_name)
                {
                    a.swap_client(handle, &model, &pc.provider_type);
                }
                a
            });

        match agent_result {
            Ok(mut new_agent) => {
                new_agent.set_chat_history(
                    Arc::clone(chat_store),
                    chat_id.to_string(),
                );
                new_agent.set_feedback_store(
                    crate::feedback::FeedbackStore::new(feedback_dir.to_path_buf()),
                );
                let sys_prompt = new_agent.system_prompt().to_string();
                let cfg = new_agent.config().clone();
                agents_map.insert(
                    chat_id,
                    Arc::new(ChatEntry {
                        agent: Mutex::new(ChatAgent {
                            agent: Some(new_agent),
                            provider_name,
                            model,
                        }),
                        messages_snapshot: tokio::sync::RwLock::new(messages),
                        system_prompt: tokio::sync::RwLock::new(sys_prompt),
                        config: tokio::sync::RwLock::new(cfg),
                        is_group,
                        message_id_map: tokio::sync::RwLock::new(HashMap::new()),
                        last_active: std::sync::atomic::AtomicI64::new(epoch_secs()),
                        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CHAT)),
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

/// Handle a callback query (inline keyboard button press).
///
/// Dispatches on the callback data prefix:
///   - `model:{provider}:{model}` — hot-swap to the selected model.
///   - `stop_agent:{id}` — cancel the selected background agent.
#[allow(clippy::too_many_arguments)]
async fn handle_callback_query(
    bot: &BotApi,
    cb_data: &str,
    chat_id: ChatId,
    agents: &Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>>,
    settings: &Settings,
    config_path: Option<&std::path::Path>,
    registry: &super::ClientRegistry,
    bg_registry: &std::sync::Arc<super::background::BackgroundAgentRegistry>,
) {
    if let Some(rest) = cb_data.strip_prefix("stop_agent:") {
        let id: u64 = match rest.parse() {
            Ok(id) => id,
            Err(_) => {
                let _ = bot.send_message(chat_id, "Invalid agent ID").await;
                return;
            }
        };
        match bg_registry.stop(id) {
            Ok(()) => {
                let _ = bot
                    .send_message(chat_id, &format!("Agent #{id} stopped."))
                    .await;
            }
            Err(e) => {
                let _ = bot.send_message(chat_id, &format!("Stop error: {e}")).await;
            }
        }
        return;
    }

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
        let agent = ca.agent.as_mut().expect("agent not available");
        agent.swap_client(handle, model, &pc.provider_type);
        ca.provider_name = provider.to_string();
        ca.model = model.to_string();
        // Update cached state for quick responses.
        let agent = ca.agent.as_ref().expect("agent not available");
        *entry.system_prompt.write().await = agent.system_prompt().to_string();
        *entry.config.write().await = agent.config().clone();
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

/// Handle a message_reaction update — record emoji feedback for RLHF.
///
/// Converts the Telegram-specific emoji reaction into a domain-level
/// `FeedbackEntry` before passing it to the store.
async fn handle_reaction(
    reaction: &types::MessageReactionUpdated,
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
) {
    let chat_id = reaction.chat.id;
    let message_id = reaction.message_id;

    // Look up the chat entry to find the message_id → turn_index mapping.
    let agents_map = agents.read().await;
    let entry = match agents_map.get(&chat_id) {
        Some(e) => Arc::clone(e),
        None => {
            tracing::debug!(chat_id, message_id, "reaction on unknown chat — ignoring");
            return;
        }
    };
    drop(agents_map);

    let id_map = entry.message_id_map.read().await;
    let turn_index = match id_map.get(&message_id) {
        Some(&idx) => idx,
        None => {
            tracing::debug!(chat_id, message_id, "reaction on unmapped message — ignoring");
            return;
        }
    };
    drop(id_map);

    // Lock the agent to record feedback.
    let ca = entry.agent.lock().await;
    let Some(ref agent) = ca.agent else {
        tracing::debug!(chat_id, "agent not available for feedback");
        return;
    };

    // Empty new_reaction means the user removed their reaction.
    if reaction.new_reaction.is_empty() {
        if let Err(e) = agent.remove_feedback(turn_index) {
            tracing::warn!(error = %e, chat_id, turn_index, "failed to remove feedback");
        } else {
            tracing::info!(chat_id, turn_index, "feedback removed (reaction cleared)");
        }
        return;
    }

    // Extract the first standard emoji from the reaction.
    let emoji = reaction
        .new_reaction
        .iter()
        .find_map(|r| {
            if r.reaction_type == "emoji" {
                r.emoji.as_deref()
            } else {
                None
            }
        });

    let Some(emoji) = emoji else {
        tracing::debug!(chat_id, message_id, "reaction has no standard emoji — ignoring");
        return;
    };

    // Convert emoji → rating at the Telegram boundary.
    let Some(rating) = feedback::emoji_to_rating(emoji) else {
        tracing::debug!(chat_id, emoji, "unknown emoji reaction — ignoring");
        return;
    };

    if let Err(e) = agent.record_feedback(turn_index, rating) {
        tracing::warn!(error = %e, chat_id, turn_index, "failed to save feedback");
    } else {
        tracing::info!(chat_id, turn_index, emoji, "feedback recorded");
    }
}

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
async fn handle_per_chat_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    entry: &Arc<ChatEntry>,
    settings: &Settings,
    config_path: Option<&std::path::Path>,
    chat_store: &dyn crate::chat_history::ChatHistory,
    registry: &super::ClientRegistry,
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
    let result = super::execute_agent_command(
        text,
        agent,
        &mut output,
        settings,
        &mut super::ModelState {
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
        super::CommandResult::Compacted => {
            (Some(agent.messages().to_vec()), None, None)
        }
        super::CommandResult::ModelSwitched { .. } => {
            (None, Some(agent.system_prompt().to_string()), Some(agent.config().clone()))
        }
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
        super::CommandResult::Cleared => {
            *entry.messages_snapshot.write().await = Vec::new();
            entry.message_id_map.write().await.clear();
            let _ = chat_store.rotate(&chat_id.0.to_string());
            tracing::info!(chat_id = chat_id.0, "conversation rotated and cleared");
        }
        super::CommandResult::Compacted => {
            let msgs = snapshot_msgs.expect("set above for Compacted");
            if let Err(e) = chat_store.save(&chat_id.0.to_string(), &msgs) {
                tracing::error!(error = %e, "failed to save chat history");
            }
            *entry.messages_snapshot.write().await = msgs;
            tracing::info!(chat_id = chat_id.0, "conversation compacted");
        }
        super::CommandResult::ModelSwitched { .. } => {
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

/// Run the agent for a message in a background task, with quick-response fallback.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_message(
    bot: BotApi,
    chat_id: ChatId,
    msg: types::Message,
    text: String,
    entry: Arc<ChatEntry>,
    chat_store: Arc<dyn crate::chat_history::ChatHistory>,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    download_limits: Arc<DownloadLimits>,
) {
    let chat_key = chat_id.0.to_string();

    let text = prepend_reply_context(&msg, text);

    // try_lock() is the gate: if the agent is busy (or temporarily extracted
    // by handle_per_chat_command), fall back to a quick response.
    let mut ca = match entry.agent.try_lock() {
        Ok(guard) if guard.agent.is_some() => guard,
        _ => {
            tracing::info!(chat_id = chat_id.0, "agent busy — using quick response");
            send_quick_response(&bot, chat_id, &text, &entry, &client).await;
            return;
        }
    };

    let (attachments, skip_reasons) = extract_attachments(&bot, &msg, &download_limits).await;

    // If the user only sent unsupported content (e.g. a binary document with
    // no caption) and nothing else to work with, reply with the skip reasons
    // and bail out before invoking the agent.
    if attachments.is_empty() && text.trim().is_empty() && !skip_reasons.is_empty() {
        let body = skip_reasons.join("\n");
        let _ = bot.send_message(chat_id, &body).await;
        return;
    }

    // If we did successfully attach something (or have text to respond to),
    // still surface any skip reasons up-front so the user knows their binary
    // was ignored.  One short message, then the agent runs as normal.
    if !skip_reasons.is_empty() {
        let body = skip_reasons.join("\n");
        let _ = bot.send_message(chat_id, &body).await;
    }

    let agent = ca.agent.as_mut().expect("checked above");

    // Set attribution for write auditing in public agents.
    // Uses the sender's @username, falling back to their numeric user ID.
    let sender_label = msg.from.as_ref().map(|u| {
        u.username
            .clone()
            .unwrap_or_else(|| u.id.to_string())
    });
    agent.set_attribution(sender_label.as_deref()).await;

    // Update snapshot so quick responses see latest context.
    *entry.messages_snapshot.write().await = agent.messages().to_vec();

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let result = if attachments.is_empty() {
        agent.run(&text, &mut output).await
    } else {
        agent.run_with_attachments(&text, attachments, &mut output).await
    };

    if let Err(e) = result {
        tracing::error!(error = %e, "agent run failed");
        let _ = output.error(&e);
    }

    // Clear attribution so background dreams don't inherit a stale user.
    agent.set_attribution(None).await;

    // Snapshot messages, then release the lock before I/O.
    let agent = ca.agent.as_ref().expect("checked above");
    let msgs = agent.messages().to_vec();
    drop(ca);

    // Record which Telegram message IDs correspond to this assistant turn.
    // This lets us map emoji reactions back to the conversation turn index.
    let sent_ids = output.sent_message_ids();
    if !sent_ids.is_empty() {
        // Find the last assistant message index in the conversation.
        if let Some(turn_index) = msgs.iter().rposition(|m| m.role == crate::message::Role::Assistant) {
            let mut id_map = entry.message_id_map.write().await;
            for msg_id in sent_ids {
                id_map.insert(msg_id.0, turn_index);
            }
        }
    }

    if let Err(e) = chat_store.save(&chat_key, &msgs) {
        tracing::error!(error = %e, "failed to save chat history");
    }
    *entry.messages_snapshot.write().await = msgs;
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
#[allow(clippy::too_many_arguments)]
async fn handle_instant_command(
    bot: &BotApi,
    text: &str,
    chat_id: ChatId,
    settings: &Settings,
    registry: &super::ClientRegistry,
    bg_registry: &std::sync::Arc<super::background::BackgroundAgentRegistry>,
    agents: &Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
) -> Option<bool> {
    // Telegram-only commands not in the shared dispatcher.
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

    // /models gets special treatment in Telegram (inline keyboard).
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

    // Shared lock-free commands.  Background agents deliver their final
    // response back to the originating Telegram chat via the on_complete
    // callback — both as a visible message and as an appended turn in the
    // chat's persisted conversation history so the next user turn sees it.
    let bot_clone = bot.clone();
    let agents_clone = Arc::clone(agents);
    let store_clone = Arc::clone(chat_store);
    let origin_chat = chat_id;
    let on_complete: super::BackgroundCompletion = std::sync::Arc::new(move |id, result| {
        let bot = bot_clone.clone();
        let agents = Arc::clone(&agents_clone);
        let store = Arc::clone(&store_clone);
        tokio::spawn(async move {
            let formatted = super::format_background_result(id, &result);
            for part in self::formatting::split_for_telegram(&formatted) {
                let _ = bot.send_message(origin_chat, &part).await;
            }
            if let Err(e) =
                append_background_result_to_chat(&agents, &*store, origin_chat, id, &result).await
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
    let result = super::execute_lockfree_command(
        text,
        settings,
        registry,
        bg_registry,
        Some(on_complete),
    )
    .await;
    if matches!(result, super::CommandResult::NotHandled) {
        return None;
    }
    render_command_result_telegram(bot, chat_id, &result).await;
    Some(true)
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
        super::persist_background_result(chat_store, &chat_key, id, result)?;
        return Ok(());
    };

    let mut ca = entry.agent.lock().await;
    let msgs = if let Some(agent) = ca.agent.as_mut() {
        let mut msgs = agent.messages().to_vec();
        msgs.push(crate::message::Message::user(
            &super::format_background_result(id, result),
        ));
        agent.set_messages(msgs.clone());
        msgs
    } else {
        // Agent temporarily extracted (e.g. during /compact) — persist via
        // chat_store and let the rebuild pick it up.
        drop(ca);
        super::persist_background_result(chat_store, &chat_key, id, result)?;
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
    result: &super::CommandResult,
) {
    match result {
        super::CommandResult::Cleared => {
            let _ = bot.send_message(chat_id, "Context cleared.").await;
        }
        super::CommandResult::Compacted => {
            let _ = bot.send_message(chat_id, "Context compacted.").await;
        }
        super::CommandResult::CompactError(e) => {
            let _ = bot.send_message(chat_id, &format!("Compaction failed: {e}")).await;
        }
        super::CommandResult::ModelSwitched {
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
        super::CommandResult::ModelSwitchError(e) => {
            let _ = bot.send_message(chat_id, &format!("Switch error: {e}")).await;
        }
        super::CommandResult::ModelParseError(e) => {
            let _ = bot.send_message(chat_id, e).await;
        }
        super::CommandResult::ModelUsage => {
            let _ = bot
                .send_message(
                    chat_id,
                    "Usage: /model <provider> [model]  or  /model <model>",
                )
                .await;
        }
        super::CommandResult::Logs(lines) => {
            for part in format_logs_for_telegram(lines) {
                let _ = bot.send_message_html(chat_id, &part).await;
            }
        }
        super::CommandResult::LogsError(e) => {
            let _ = bot.send_message(chat_id, &format!("Logs error: {e}")).await;
        }
        super::CommandResult::LoopStarted { id, chat_id: bg_chat_id, .. } => {
            let _ = bot
                .send_message(chat_id, &format!("Agent #{id} started — chat: {bg_chat_id}"))
                .await;
        }
        super::CommandResult::LoopError(e) => {
            let _ = bot.send_message(chat_id, &format!("Loop error: {e}")).await;
        }
        super::CommandResult::AgentList { agents } => {
            if agents.is_empty() {
                let _ = bot.send_message(chat_id, "No background agents running.").await;
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
        super::CommandResult::AgentStopped { id } => {
            let _ = bot.send_message(chat_id, &format!("Agent #{id} stopped.")).await;
        }
        super::CommandResult::StopError(e) => {
            let _ = bot.send_message(chat_id, &format!("Stop error: {e}")).await;
        }
        super::CommandResult::ModelList { .. } | super::CommandResult::NotHandled => {}
    }
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

/// If the message is a reply, prepend the original message's text and sender
/// so the agent sees both the replied-to content and the new reply.
fn prepend_reply_context(msg: &types::Message, text: String) -> String {
    match msg.reply_to_message {
        Some(ref reply) => {
            let original = reply
                .text
                .as_deref()
                .or(reply.caption.as_deref())
                .unwrap_or("[no text]");
            let sender = reply
                .from
                .as_ref()
                .and_then(|u| u.username.as_deref())
                .unwrap_or("unknown");
            format!("[Replying to message from @{sender}: \"{original}\"]\n\n{text}")
        }
        None => text,
    }
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
/// Shared context for creating new chat entries.
struct ChatContext<'a> {
    settings: &'a Settings,
    controller_prompt: Option<&'a str>,
    chat_store: &'a Arc<dyn crate::chat_history::ChatHistory>,
    feedback_dir: &'a std::path::Path,
    registry: &'a super::ClientRegistry,
}

async fn get_or_create_entry(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    chat_id: i64,
    is_group: bool,
    cx: &ChatContext<'_>,
) -> crate::Result<Arc<ChatEntry>> {
    // Fast path: entry already exists.
    {
        let map = agents.read().await;
        if let Some(entry) = map.get(&chat_id) {
            entry.last_active.store(epoch_secs(), std::sync::atomic::Ordering::Relaxed);
            return Ok(Arc::clone(entry));
        }
    }

    // Slow path: create a new agent for this chat.
    let mode = if is_group {
        crate::controller::AgentMode::Public
    } else {
        crate::controller::AgentMode::Private
    };
    let client = cx.registry.get_default();
    let chat_key = chat_id.to_string();
    let ch = if is_group { Some(chat_key.as_str()) } else { None };
    let mut agent =
        crate::controller::build_agent(cx.settings, cx.controller_prompt, mode, client, cx.registry, ch).await?;

    // Attach chat history so compaction can rotate pre-compaction snapshots.
    agent.set_chat_history(Arc::clone(cx.chat_store), chat_key.clone());
    agent.set_feedback_store(crate::feedback::FeedbackStore::new(cx.feedback_dir.to_path_buf()));

    let mut restored_messages = Vec::new();
    if let Ok(messages) = cx.chat_store.load(&chat_key)
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
        super::active_provider_name(cx.settings).unwrap_or_default();
    let model = cx.settings.agent.model.clone();
    let sys_prompt = agent.system_prompt().to_string();
    let config = agent.config().clone();

    let entry = Arc::new(ChatEntry {
        agent: Mutex::new(ChatAgent {
            agent: Some(agent),
            provider_name,
            model,
        }),
        messages_snapshot: tokio::sync::RwLock::new(restored_messages),
        system_prompt: tokio::sync::RwLock::new(sys_prompt),
        config: tokio::sync::RwLock::new(config),
        is_group,
        message_id_map: tokio::sync::RwLock::new(HashMap::new()),
        last_active: std::sync::atomic::AtomicI64::new(epoch_secs()),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CHAT)),
    });

    // Evict the least-recently-active entry if we're at capacity.
    // Conversation history is already persisted to chat_store on every
    // turn, so the evicted chat can be fully restored on next message.
    let mut map = agents.write().await;
    if map.len() >= MAX_CHAT_ENTRIES && !map.contains_key(&chat_id)
        && let Some((&victim_id, _)) = map
            .iter()
            .min_by_key(|(_, e)| e.last_active.load(std::sync::atomic::Ordering::Relaxed))
        {
            tracing::info!(
                evicted_chat_id = victim_id,
                active_chats = map.len(),
                "evicting least-recently-active chat entry"
            );
            map.remove(&victim_id);
        }
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

/// Build an inline keyboard listing all running background agents as
/// tap-to-stop buttons.
///
/// Each button shows `⏹ Stop #{id}: {preview} ({elapsed}s)`.  The callback
/// data encodes `stop_agent:{id}` so the handler can cancel the selected
/// agent via the `BackgroundAgentRegistry`.
fn build_agents_keyboard(
    agents: &[super::background::BackgroundAgentListEntry],
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
fn truncate_chars(s: &str, max: usize) -> String {
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

// ---------------------------------------------------------------------------
// Media extraction helpers
// ---------------------------------------------------------------------------

/// Download media attachments from a Telegram message.
///
/// Only handles Telegram-specific concerns: detecting media types from
/// message fields and downloading via the Bot API.  Media resolution
/// (resizing, transcription, PDF extraction) is handled by the agent
/// via `run_with_attachments()`.
async fn extract_attachments(
    bot: &BotApi,
    msg: &types::Message,
    limits: &DownloadLimits,
) -> (Vec<media::Attachment>, Vec<String>) {
    let mut attachments = Vec::new();
    let mut skip_reasons: Vec<String> = Vec::new();

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
        match bot.download_file(&photo.file_id, limits.image_max_bytes).await {
            Ok(data) => {
                attachments.push(media::Attachment {
                    data,
                    mime_type: "image/jpeg".into(),
                    file_name: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "failed to download photo"),
        }
    }

    // Voice notes.
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
        match bot.download_file(&voice.file_id, limits.audio_max_bytes).await {
            Ok(data) => {
                attachments.push(media::Attachment {
                    data,
                    mime_type: mime,
                    file_name: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "failed to download voice note"),
        }
    }

    // Documents: images, PDFs, Office, and text-like files.  Binaries are
    // rejected here without being downloaded at all.
    if let Some(doc) = &msg.document {
        let mime = doc.mime_type.as_deref().unwrap_or("").to_string();
        let file_name = doc.file_name.clone();
        let display_name = file_name.as_deref().unwrap_or("file").to_string();

        let kind = classify_document(&mime, file_name.as_deref());
        if matches!(kind, DocumentKind::Binary) {
            tracing::info!(
                file_name = display_name.as_str(),
                mime_type = mime.as_str(),
                "skipping binary document (not downloaded)"
            );
            skip_reasons.push(format!(
                "Skipped `{display_name}` — I can only read text files, Office docs, PDFs, and images."
            ));
        } else {
            let limit = match kind {
                DocumentKind::Image => limits.image_max_bytes,
                DocumentKind::Text => limits.text_max_bytes,
                _ => limits.document_max_bytes,
            };
            tracing::info!(
                file_id = doc.file_id.as_str(),
                file_name = display_name.as_str(),
                mime_type = mime.as_str(),
                kind = ?kind,
                "downloading document from Telegram"
            );
            match bot.download_file(&doc.file_id, limit).await {
                Ok(data) => {
                    // Text documents need UTF-8 validation; reject if invalid.
                    if matches!(kind, DocumentKind::Text) && std::str::from_utf8(&data).is_err() {
                        tracing::warn!(file_name = display_name.as_str(), "not valid UTF-8 — dropping");
                        skip_reasons.push(format!(
                            "Skipped `{display_name}` — looked like text but isn't valid UTF-8."
                        ));
                    } else {
                        let effective_mime = resolve_effective_mime(&mime, kind, file_name.as_deref());
                        attachments.push(media::Attachment {
                            data,
                            mime_type: effective_mime,
                            file_name,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to download document");
                    skip_reasons.push(format!("Couldn't download `{display_name}`: {e}"));
                }
            }
        }
    }

    (attachments, skip_reasons)
}

/// What kind of Telegram document we're looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentKind {
    Image,
    Pdf,
    Office,
    Text,
    Binary,
}

/// Classify a document by its MIME type, falling back to the filename
/// extension when the MIME is missing or generic (`application/octet-stream`).
fn classify_document(mime: &str, file_name: Option<&str>) -> DocumentKind {
    use crate::media::{is_office_extension, is_office_mime, is_text_extension, is_text_like_mime};

    if mime.starts_with("image/") {
        return DocumentKind::Image;
    }
    if mime == "application/pdf" {
        return DocumentKind::Pdf;
    }
    if is_office_mime(mime) {
        return DocumentKind::Office;
    }
    if is_text_like_mime(mime) {
        return DocumentKind::Text;
    }
    // Fall back to extension when MIME is empty / octet-stream / unknown.
    // Telegram often labels source files as application/octet-stream.
    if matches!(mime, "" | "application/octet-stream")
        && let Some(ext) = file_name.and_then(extension_of)
    {
        if is_office_extension(&ext) {
            return DocumentKind::Office;
        }
        if is_text_extension(&ext) {
            return DocumentKind::Text;
        }
    }
    DocumentKind::Binary
}

/// Lowercase extension (without the dot) of a filename, if any.
fn extension_of(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

/// When Telegram's MIME type is empty / `application/octet-stream` but we
/// classified the document by extension, resolve to the correct MIME so the
/// media resolver can route it properly.
fn resolve_effective_mime(original: &str, kind: DocumentKind, file_name: Option<&str>) -> String {
    match kind {
        DocumentKind::Office if !crate::media::is_office_mime(original) => {
            file_name
                .and_then(extension_of)
                .and_then(|ext| match ext.as_str() {
                    "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
                    "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
                    "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
                    "doc" => Some("application/msword"),
                    "xls" => Some("application/vnd.ms-excel"),
                    "ppt" => Some("application/vnd.ms-powerpoint"),
                    _ => None,
                })
                .unwrap_or(original)
                .to_string()
        }
        DocumentKind::Text if !crate::media::is_text_like_mime(original) => {
            "text/plain".to_string()
        }
        _ => original.to_string(),
    }
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
    fn classify_image_mime() {
        assert_eq!(
            classify_document("image/png", Some("a.png")),
            DocumentKind::Image
        );
        assert_eq!(
            classify_document("image/jpeg", None),
            DocumentKind::Image
        );
    }

    #[test]
    fn classify_pdf() {
        assert_eq!(
            classify_document("application/pdf", Some("paper.pdf")),
            DocumentKind::Pdf
        );
    }

    #[test]
    fn classify_text_mime() {
        assert_eq!(
            classify_document("text/plain", Some("note.txt")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("text/markdown", Some("README.md")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("application/json", Some("pkg.json")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("application/x-yaml", Some("c.yaml")),
            DocumentKind::Text
        );
    }

    #[test]
    fn classify_octet_stream_by_extension() {
        assert_eq!(
            classify_document("application/octet-stream", Some("main.rs")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("", Some("Cargo.toml")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("application/octet-stream", Some("x.py")),
            DocumentKind::Text
        );
        assert_eq!(
            classify_document("application/octet-stream", Some("deploy.sh")),
            DocumentKind::Text
        );
    }

    #[test]
    fn classify_office_mime() {
        assert_eq!(
            classify_document(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                Some("doc.docx")
            ),
            DocumentKind::Office
        );
        assert_eq!(
            classify_document(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                Some("data.xlsx")
            ),
            DocumentKind::Office
        );
        assert_eq!(
            classify_document(
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                Some("deck.pptx")
            ),
            DocumentKind::Office
        );
    }

    #[test]
    fn classify_office_by_extension() {
        assert_eq!(
            classify_document("application/octet-stream", Some("report.docx")),
            DocumentKind::Office
        );
        assert_eq!(
            classify_document("", Some("budget.xlsx")),
            DocumentKind::Office
        );
        assert_eq!(
            classify_document("application/octet-stream", Some("slides.pptx")),
            DocumentKind::Office
        );
    }

    #[test]
    fn classify_binary_defaults() {
        assert_eq!(
            classify_document("application/zip", Some("a.zip")),
            DocumentKind::Binary
        );
        assert_eq!(
            classify_document("video/mp4", Some("clip.mp4")),
            DocumentKind::Binary
        );
        assert_eq!(
            classify_document("application/octet-stream", Some("thing.bin")),
            DocumentKind::Binary
        );
        assert_eq!(classify_document("", None), DocumentKind::Binary);
        assert_eq!(
            classify_document("application/octet-stream", None),
            DocumentKind::Binary
        );
    }

    #[test]
    fn extension_extraction() {
        assert_eq!(extension_of("README.md"), Some("md".into()));
        assert_eq!(extension_of("foo.TAR.GZ"), Some("gz".into()));
        assert_eq!(extension_of("nodot"), None);
        assert_eq!(extension_of("trailing."), None);
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
        use crate::workspace::Workspace;

        // Public agents now get identity via workspace.system_prompt(),
        // which is composed from SOUL.md and IDENTITY.md by the workspace.
        // Here we verify the prompt composition order works correctly.
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("SOUL.md", "I speak like a pirate.")
            .with_file("IDENTITY.md", "I am Captain Bot.");

        let mut agent_settings = crate::config::AgentSettings::default();

        // Workspace system prompt provides identity.
        let ws_prompt = ws.system_prompt();
        if !ws_prompt.is_empty() {
            agent_settings.system_prompt.push_str("\n\n");
            agent_settings.system_prompt.push_str(&ws_prompt);
        }

        // Public-agent suffix.
        agent_settings.system_prompt.push_str(
            "\n\nYou are a public-facing agent.",
        );

        // Telegram controller prompt.
        let telegram_prompt = "You are responding via Telegram. Keep these rules:\n\
             - Keep responses concise. Telegram messages have a 4096 character limit.";
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(telegram_prompt);

        let prompt = &agent_settings.system_prompt;
        // Identity content from workspace system prompt.
        assert!(prompt.contains("I speak like a pirate."), "should contain SOUL.md content");
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
            download_limits: Arc::new(DownloadLimits::default()),
        };
        let prompt = ctrl.system_prompt().expect("Telegram controller must provide a system prompt");
        assert!(prompt.contains("Telegram"), "should reference Telegram");
        assert!(prompt.contains("4096"), "should mention the message character limit");
    }

    // -------------------------------------------------------------------
    // Reply context — prepend_reply_context
    // -------------------------------------------------------------------

    /// Helper to build a reply Message with the given text and sender username.
    fn make_reply(text: Option<&str>, username: Option<&str>) -> Message {
        Message {
            message_id: 0,
            chat: Chat {
                id: 100,
                chat_type: ChatType::Private,
            },
            from: username.map(|u| User {
                id: 1,
                is_bot: false,
                username: Some(u.to_string()),
            }),
            text: text.map(std::string::ToString::to_string),
            caption: None,
            entities: None,
            reply_to_message: None,
            photo: None,
            voice: None,
            document: None,
        }
    }

    #[test]
    fn reply_context_includes_original_text_and_sender() {
        let mut msg = make_msg("my reply", ChatType::Private);
        msg.reply_to_message = Some(Box::new(make_reply(
            Some("original message"),
            Some("alice"),
        )));
        let result = prepend_reply_context(&msg, "my reply".to_string());
        assert_eq!(
            result,
            "[Replying to message from @alice: \"original message\"]\n\nmy reply",
        );
    }

    #[test]
    fn reply_context_uses_caption_when_no_text() {
        let mut reply = make_reply(None, Some("bob"));
        reply.caption = Some("photo caption".to_string());

        let mut msg = make_msg("nice pic", ChatType::Private);
        msg.reply_to_message = Some(Box::new(reply));

        let result = prepend_reply_context(&msg, "nice pic".to_string());
        assert!(result.contains("\"photo caption\""));
        assert!(result.ends_with("nice pic"));
    }

    #[test]
    fn reply_context_falls_back_to_no_text() {
        let mut msg = make_msg("what was that?", ChatType::Private);
        msg.reply_to_message = Some(Box::new(make_reply(None, Some("carol"))));

        let result = prepend_reply_context(&msg, "what was that?".to_string());
        assert!(result.contains("[no text]"));
    }

    #[test]
    fn reply_context_falls_back_to_unknown_sender() {
        let mut reply = make_reply(Some("hello"), None);
        reply.from = None;

        let mut msg = make_msg("hey", ChatType::Private);
        msg.reply_to_message = Some(Box::new(reply));

        let result = prepend_reply_context(&msg, "hey".to_string());
        assert!(result.contains("@unknown"));
    }

    #[test]
    fn no_reply_returns_text_unchanged() {
        let msg = make_msg("hello", ChatType::Private);
        let result = prepend_reply_context(&msg, "hello".to_string());
        assert_eq!(result, "hello");
    }

    #[test]
    fn reply_context_preserves_empty_reply_text() {
        let mut msg = make_msg("", ChatType::Private);
        msg.reply_to_message = Some(Box::new(make_reply(Some("original"), Some("dave"))));

        let result = prepend_reply_context(&msg, String::new());
        assert!(result.contains("\"original\""));
        assert!(result.ends_with("\n\n"));
    }

    #[test]
    fn truncate_chars_short_string_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_long_string_shortened_with_ellipsis() {
        let out = truncate_chars("abcdefghij", 5);
        assert_eq!(out, "abcd…");
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_chars_handles_multibyte() {
        // Each of these is multi-byte in UTF-8 but one char.
        let out = truncate_chars("αβγδεζηθ", 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn agents_keyboard_has_stop_button_per_agent() {
        use std::time::Duration;
        let agents = vec![
            super::super::background::BackgroundAgentListEntry {
                id: 1,
                prompt_preview: "fix bug".into(),
                elapsed: Duration::from_secs(5),
                chat_id: "bg-1".into(),
            },
            super::super::background::BackgroundAgentListEntry {
                id: 2,
                prompt_preview: "write docs".into(),
                elapsed: Duration::from_secs(30),
                chat_id: "bg-2".into(),
            },
        ];
        let keyboard = build_agents_keyboard(&agents);
        assert_eq!(keyboard.inline_keyboard.len(), 2);
        for (i, row) in keyboard.inline_keyboard.iter().enumerate() {
            assert_eq!(row.len(), 1);
            let btn = &row[0];
            // The word "Stop" must be visible so the action is obvious.
            assert!(btn.text.contains("Stop"), "button text: {}", btn.text);
            assert!(btn.text.contains(&format!("#{}", agents[i].id)));
            assert_eq!(
                btn.callback_data.as_deref(),
                Some(format!("stop_agent:{}", agents[i].id).as_str()),
            );
        }
    }

    #[test]
    fn agents_keyboard_empty_when_no_agents() {
        let keyboard = build_agents_keyboard(&[]);
        assert!(keyboard.inline_keyboard.is_empty());
    }
}
