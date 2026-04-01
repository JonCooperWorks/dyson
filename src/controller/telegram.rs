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
//     ├── create teloxide Bot from bot_token
//     ├── teloxide polling loop:
//     │     ├── receive Message from Telegram
//     │     ├── check allowed_chat_ids (access control)
//     │     ├── create Agent + TelegramOutput for this message
//     │     └── agent.run(text, &mut output)
//     │           ├── output.text_delta("Hello") → edit message
//     │           └── output.flush()             → final edit
//     └── runs until shutdown
//
// The block_on problem:
//   The `Output` trait is sync (for terminal compatibility), but teloxide
//   is async.  We can't use `Handle::block_on()` because we're already
//   inside a tokio runtime.  Instead, we use `tokio::task::block_in_place`
//   with `Handle::block_on()` — this moves the blocking call off the
//   async worker thread onto a blocking thread, then executes the async
//   teloxide call there.  This is the correct bridge pattern.
// ===========================================================================

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{
    ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, MessageId, ParseMode,
};
use tokio::sync::Mutex;

use serde::Deserialize;

use crate::config::{ControllerConfig, Settings};
use crate::controller::Output;
use crate::error::DysonError;
use crate::media;
use crate::message::ContentBlock;
use crate::tool::ToolOutput;

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
             be delivered as a Telegram document automatically."
        )
    }

    async fn run(&self, settings: &Settings) -> crate::Result<()> {
        eprintln!(
            "Dyson v{} — running as Telegram bot",
            env!("CARGO_PKG_VERSION")
        );

        let bot = teloxide::Bot::new(self.bot_token.expose());
        let allowed_ids = self.allowed_chat_ids.clone();
        let mut current_settings = settings.clone();
        let controller_prompt = self.system_prompt().map(|s| s.to_string());

        // Hot reload: watch config + workspace files.
        let config_path = std::env::args()
            .skip_while(|a| a != "--config" && a != "-c")
            .nth(1)
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let p = std::path::PathBuf::from("dyson.json");
                if p.exists() { Some(p) } else { None }
            });
        let workspace_path = crate::workspace::OpenClawWorkspace::resolve_path(
            Some(settings.workspace.connection_string.expose()),
        );
        let mut reloader = crate::config::hot_reload::HotReloader::new(
            config_path.as_deref(),
            workspace_path.as_deref(),
        );

        // Per-chat agents — persistent conversation context.
        // Each chat gets its own agent that remembers previous messages.
        // /clear resets a chat's agent and deletes persisted history.
        // ChatAgent tracks the active provider/model for within-provider switching.
        let agents: Arc<Mutex<HashMap<i64, ChatAgent>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Chat store — persists conversation history via the configured
        // backend.  Uses the ChatHistory trait so the backend can be
        // swapped (disk, database, RAG, etc.).
        let chat_store: Arc<dyn crate::chat_history::ChatHistory> = {
            let store = crate::chat_history::create_chat_history(&settings.chat_history)?;
            Arc::from(store)
        };


        // Manual polling loop instead of teloxide::repl.
        // teloxide::repl swallows SIGINT and can't be Ctrl-C'd.
        let mut offset: i64 = 0;

        loop {
            // Check for config/workspace changes each poll cycle.
            if let Ok((true, new_settings)) = reloader.check() {
                if let Some(s) = new_settings {
                    current_settings = s;
                    current_settings.dangerous_no_sandbox = settings.dangerous_no_sandbox;
                }
                // Clear all agents so they pick up new config.
                agents.lock().await.clear();
                tracing::info!("config/workspace reloaded — agents reset");
            }
            // Poll for updates with a timeout, racing against Ctrl-C.
            let updates = tokio::select! {
                result = bot.get_updates().offset(offset as i32).timeout(30).send() => {
                    match result {
                        Ok(updates) => updates,
                        Err(e) => {
                            tracing::warn!(error = %e, "getUpdates failed — retrying");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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
                offset = i64::from(update.id.0) + 1;

                // Handle callback queries (inline keyboard button presses).
                if let teloxide::types::UpdateKind::CallbackQuery(cb) = &update.kind {
                    let cb_chat_id = cb.message.as_ref().map(|m| m.chat().id);
                    let cb_data = cb.data.clone().unwrap_or_default();

                    // Acknowledge the callback to remove the loading spinner.
                    let _ = bot.answer_callback_query(cb.id.clone()).await;

                    if let Some(chat_id) = cb_chat_id {
                        // Check access control.
                        if !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id.0) {
                            continue;
                        }

                        // Parse callback data: "model:{provider}:{model}"
                        if let Some(rest) = cb_data.strip_prefix("model:") {
                            if let Some((provider, model)) = rest.split_once(':') {
                                let existing_messages = {
                                    let agents_map = agents.lock().await;
                                    agents_map
                                        .get(&chat_id.0)
                                        .map(|ca| ca.agent.messages().to_vec())
                                        .unwrap_or_default()
                                };
                                match super::build_agent_with_provider(
                                    &current_settings,
                                    provider,
                                    Some(model),
                                    controller_prompt.as_deref(),
                                    existing_messages,
                                )
                                .await
                                {
                                    Ok(new_agent) => {
                                        let pc = &current_settings.providers[provider];
                                        let reply = format!(
                                            "Switched to '{}' — {:?} ({})",
                                            provider, pc.provider_type, model,
                                        );
                                        agents.lock().await.insert(chat_id.0, ChatAgent {
                                            agent: new_agent,
                                            provider_name: provider.to_string(),
                                            model: model.to_string(),
                                        });
                                        let _ = bot.send_message(chat_id, reply).await;
                                    }
                                    Err(e) => {
                                        let _ = bot
                                            .send_message(chat_id, format!("Switch error: {e}"))
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }

                // Extract the Telegram message.
                let msg = match &update.kind {
                    teloxide::types::UpdateKind::Message(m) => m.clone(),
                    _ => continue,
                };

                // Extract text from either text or caption (for media messages).
                // Commands are only parsed from text, not media-only messages.
                let text = msg.text().or(msg.caption())
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_string());

                // Check if the message has any processable content at all.
                let has_media = msg.photo().is_some()
                    || msg.voice().is_some()
                    || msg.document().map_or(false, |d| {
                        d.mime_type.as_ref().map_or(false, |m| m.as_ref().starts_with("image/"))
                    });

                if text.is_none() && !has_media {
                    continue;
                }

                // Unwrap text for command parsing (empty string if media-only).
                let text = text.unwrap_or_default();

                let chat_id = msg.chat.id;

                // /whoami — respond immediately, no LLM needed.
                if text == "/whoami" {
                    let _ = bot.send_message(chat_id, chat_id.0.to_string()).await;
                    continue;
                }

                // Access control.
                if !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id.0) {
                    tracing::warn!(chat_id = chat_id.0, "unauthorized chat — ignoring");
                    continue;
                }

                // /clear — rotate conversation history and start fresh.
                // Agent::clear() spawns a background task to synthesise
                // learnings into workspace memory before dropping messages.
                if text == "/clear" {
                    {
                        let mut agents_map = agents.lock().await;
                        if let Some(ca) = agents_map.get_mut(&chat_id.0) {
                            ca.agent.clear();
                        }
                    }
                    agents.lock().await.remove(&chat_id.0);
                    let _ = chat_store.rotate(&chat_id.0.to_string());
                    let _ = bot.send_message(chat_id, "Context cleared.").await;
                    tracing::info!(chat_id = chat_id.0, "conversation rotated and cleared");
                    continue;
                }

                // /compact — summarise the conversation and replace the
                // history with the summary.  Keeps the agent alive (unlike
                // /clear) so the model retains a condensed version of context.
                if text == "/compact" {
                    let mut agents_map = agents.lock().await;
                    if let Some(ca) = agents_map.get_mut(&chat_id.0) {
                        let mut output = TelegramOutput::new(bot.clone(), chat_id);
                        match ca.agent.compact(&mut output).await {
                            Ok(()) => {
                                let chat_key = chat_id.0.to_string();
                                let _ = chat_store.save(&chat_key, ca.agent.messages());
                                let _ = bot.send_message(chat_id, "Context compacted.").await;
                                tracing::info!(chat_id = chat_id.0, "conversation compacted");
                            }
                            Err(e) => {
                                let _ = bot.send_message(chat_id, format!("Compaction failed: {e}")).await;
                                tracing::error!(error = %e, "compaction failed");
                            }
                        }
                    } else {
                        let _ = bot.send_message(chat_id, "No active conversation to compact.").await;
                    }
                    continue;
                }

                // /memory — save a note to the workspace memory.
                if let Some(note) = text.strip_prefix("/memory ") {
                    let note = note.trim();
                    if note.is_empty() {
                        let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
                        continue;
                    }
                    match save_memory_note(
                        &current_settings,
                        note,
                    ) {
                        Ok(()) => {
                            let _ = bot.send_message(chat_id, "Saved to memory.").await;
                            tracing::info!(chat_id = chat_id.0, "memory note saved");
                        }
                        Err(e) => {
                            let _ = bot.send_message(chat_id, format!("Error: {e}")).await;
                            tracing::error!(error = %e, "failed to save memory note");
                        }
                    }
                    continue;
                }
                if text == "/memory" {
                    let _ = bot.send_message(chat_id, "Usage: /memory <note>").await;
                    continue;
                }

                // /models — list available providers and models as clickable buttons.
                if text == "/models" {
                    if current_settings.providers.is_empty() {
                        let _ = bot.send_message(chat_id, "No providers configured.").await;
                    } else {
                        let (cp, cm) = {
                            let agents_map = agents.lock().await;
                            match agents_map.get(&chat_id.0) {
                                Some(ca) => (ca.provider_name.clone(), ca.model.clone()),
                                None => {
                                    let pn = super::active_provider_name(&current_settings)
                                        .unwrap_or_default();
                                    let m = current_settings.agent.model.clone();
                                    (pn, m)
                                }
                            }
                        };
                        let keyboard = build_model_keyboard(
                            &current_settings, &cp, &cm,
                        );
                        let _ = bot
                            .send_message(chat_id, "Select a model:")
                            .reply_markup(keyboard)
                            .await;
                    }
                    continue;
                }

                // /model <provider> [model] — switch provider and/or model.
                if let Some(args) = text.strip_prefix("/model ").map(str::trim) {
                    if args.is_empty() {
                        let _ = bot.send_message(chat_id, "Usage: /model <provider> [model]  or  /model <model>").await;
                        continue;
                    }
                    let current_prov = {
                        let agents_map = agents.lock().await;
                        agents_map.get(&chat_id.0)
                            .map(|ca| ca.provider_name.clone())
                            .unwrap_or_else(|| {
                                super::active_provider_name(&current_settings)
                                    .unwrap_or_default()
                            })
                    };
                    let (target_provider, target_model) = match super::parse_model_command(
                        args,
                        &current_settings.providers,
                        &current_prov,
                    ) {
                        Ok(parsed) => parsed,
                        Err(e) => {
                            let _ = bot.send_message(chat_id, e).await;
                            continue;
                        }
                    };
                    let existing_messages = {
                        let agents_map = agents.lock().await;
                        agents_map
                            .get(&chat_id.0)
                            .map(|ca| ca.agent.messages().to_vec())
                            .unwrap_or_default()
                    };
                    match super::build_agent_with_provider(
                        &current_settings,
                        &target_provider,
                        target_model.as_deref(),
                        controller_prompt.as_deref(),
                        existing_messages,
                    )
                    .await
                    {
                        Ok(new_agent) => {
                            let pc = &current_settings.providers[&target_provider];
                            let resolved = target_model.as_deref()
                                .unwrap_or_else(|| pc.default_model())
                                .to_string();
                            let reply = format!(
                                "Switched to '{}' — {:?} ({})",
                                target_provider, pc.provider_type, resolved,
                            );
                            agents.lock().await.insert(chat_id.0, ChatAgent {
                                agent: new_agent,
                                provider_name: target_provider,
                                model: resolved,
                            });
                            let _ = bot.send_message(chat_id, reply).await;
                        }
                        Err(e) => {
                            let _ = bot
                                .send_message(chat_id, format!("Switch error: {e}"))
                                .await;
                        }
                    }
                    continue;
                }
                if text == "/model" {
                    let _ = bot
                        .send_message(chat_id, "Usage: /model <provider> [model]  or  /model <model>")
                        .await;
                    continue;
                }

                tracing::info!(chat_id = chat_id.0, "telegram message received");

                // Spawn the agent run in a background task so the polling
                // loop doesn't block.  Without this, a slow LLM response
                // freezes the entire bot — no new messages are received
                // until the current one finishes.
                let bot_clone = bot.clone();
                let settings_clone = current_settings.clone();
                let prompt_clone = controller_prompt.clone();
                let agents_clone = agents.clone();
                let store_clone = chat_store.clone();
                tokio::spawn(async move {
                    let chat_key = chat_id.0.to_string();

                    // -- Resolve media into ContentBlocks --
                    let model_name = settings_clone.agent.model.clone();
                    let extracted = match extract_content(
                        &bot_clone, &msg, &text, &model_name,
                    ).await {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::error!(error = %e, "failed to extract media content");
                            let _ = bot_clone.send_message(chat_id, format!("Media error: {e}")).await;
                            return;
                        }
                    };

                    // Notify the user about unsupported media.
                    let has_unsupported = !extracted.unsupported.is_empty();
                    for msg in &extracted.unsupported {
                        let _ = bot_clone.send_message(chat_id, msg).await;
                    }

                    // If there are no content blocks left, nothing to send.
                    if extracted.blocks.is_empty() {
                        return;
                    }

                    let content_blocks = extracted.blocks;

                    // Get or create the per-chat agent.
                    let mut agents_map = agents_clone.lock().await;
                    if let std::collections::hash_map::Entry::Vacant(entry) = agents_map.entry(chat_id.0) {
                        let provider_name = super::active_provider_name(&settings_clone)
                            .unwrap_or_default();
                        let model = settings_clone.agent.model.clone();
                        match crate::controller::build_agent(
                            &settings_clone,
                            prompt_clone.as_deref(),
                        ).await {
                            Ok(mut agent) => {
                                // Restore conversation history from disk.
                                if let Ok(messages) = store_clone.load(&chat_key) && !messages.is_empty() {
                                    tracing::info!(
                                        chat_id = chat_id.0,
                                        messages = messages.len(),
                                        "restored chat history"
                                    );
                                    agent.set_messages(messages);
                                }
                                entry.insert(ChatAgent {
                                    agent,
                                    provider_name,
                                    model,
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to create agent");
                                let _ = bot_clone.send_message(chat_id, format!("Error: {e}")).await;
                                return;
                            }
                        }
                    }
                    let ca = agents_map.get_mut(&chat_id.0)
                        .expect("agent must exist — just inserted above");

                    let mut output = TelegramOutput::new(bot_clone.clone(), chat_id);

                    // Use run_with_blocks for multimodal, run for text-only.
                    let has_non_text = content_blocks.iter().any(|b| !matches!(b, ContentBlock::Text { .. }));
                    let result = if has_non_text {
                        ca.agent.run_with_blocks(content_blocks, &mut output).await
                    } else {
                        ca.agent.run(&text, &mut output).await
                    };

                    if let Err(e) = result {
                        tracing::error!(error = %e, "agent run failed");
                        let _ = bot_clone.send_message(chat_id, format!("Error: {e}")).await;
                    }

                    // Persist conversation history to disk after each turn,
                    // but skip if the message contained unsupported media.
                    if !has_unsupported {
                        if let Err(e) = store_clone.save(&chat_key, ca.agent.messages()) {
                            tracing::error!(error = %e, "failed to save chat history");
                        }
                    }

                    drop(agents_map);
                });
            }
        }

        Ok(())
    }
}

/// Build an inline keyboard listing all providers and models.
///
/// Each button shows the model name (with a check mark for the active one).
/// The callback data encodes `model:{provider}:{model}` so the handler
/// can switch to the selected model.
fn build_model_keyboard(
    settings: &Settings,
    current_provider: &str,
    current_model: &str,
) -> InlineKeyboardMarkup {
    let mut providers: Vec<_> = settings.providers.iter().collect();
    providers.sort_by_key(|(name, _)| name.as_str());

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (name, pc) in &providers {
        // Provider header row (non-clickable label).
        let label = format!("{} — {:?}", name, pc.provider_type);
        // Use a callback with empty payload so tapping the header is a no-op.
        rows.push(vec![InlineKeyboardButton::callback(label, "noop")]);

        for model in &pc.models {
            let active = *name == current_provider && model == current_model;
            let display = if active {
                format!("✓ {model}")
            } else {
                model.clone()
            };
            let data = format!("model:{name}:{model}");
            rows.push(vec![InlineKeyboardButton::callback(display, data)]);
        }
    }

    InlineKeyboardMarkup::new(rows)
}

/// Save a note to the workspace MEMORY.md file.
fn save_memory_note(
    settings: &Settings,
    note: &str,
) -> crate::Result<()> {
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
/// Handles text, photos, voice notes, and image documents.  Text from
/// `msg.text()` or `msg.caption()` is combined with resolved media
/// blocks into a single Vec<ContentBlock>.
/// Result of extracting content from a Telegram message.
struct ExtractedContent {
    /// Content blocks to send to the agent.
    blocks: Vec<ContentBlock>,
    /// Friendly messages about unsupported media (sent to user, not saved to history).
    unsupported: Vec<String>,
}

async fn extract_content(
    bot: &Bot,
    msg: &teloxide::types::Message,
    text: &str,
    model: &str,
) -> crate::Result<ExtractedContent> {
    let mut blocks = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();

    // Add text content if present.
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    // Photos: pick the largest resolution (last in the array).
    if let Some(photos) = msg.photo() {
        if let Some(photo) = photos.last() {
            tracing::info!(
                file_id = photo.file.id.0.as_str(),
                width = photo.width,
                height = photo.height,
                "downloading photo from Telegram"
            );
            match download_telegram_file(bot, &photo.file.id.0).await {
                Ok(data) => {
                    match media::resolve(media::MediaInput::Image {
                        data,
                        mime_type: "image/jpeg".to_string(),
                    }).await {
                        Ok(media::ResolvedMedia::Images(image_blocks)) => {
                            blocks.extend(image_blocks);
                        }
                        Ok(media::ResolvedMedia::Transcription(t)) => {
                            blocks.push(ContentBlock::Text { text: t });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to process photo");
                            unsupported.push(format!("{model} doesn't support vision"));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to download photo");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Failed to download photo: {e}]"),
                    });
                }
            }
        }
    }

    // Voice notes: transcribe via Whisper.
    if let Some(voice) = msg.voice() {
        tracing::info!(
            file_id = voice.file.id.0.as_str(),
            "downloading voice note from Telegram"
        );
        let mime = voice.mime_type.as_ref()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "audio/ogg".to_string());

        match download_telegram_file(bot, &voice.file.id.0).await {
            Ok(data) => {
                match media::resolve(media::MediaInput::Audio {
                    data,
                    mime_type: mime,
                }).await {
                    Ok(media::ResolvedMedia::Transcription(t)) => {
                        blocks.push(ContentBlock::Text {
                            text: format!("[Voice transcription]: {t}"),
                        });
                    }
                    Ok(media::ResolvedMedia::Images(_)) => {
                        // Shouldn't happen for audio, but handle gracefully.
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to transcribe voice note");
                        unsupported.push(format!("{model} doesn't support audio"));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to download voice note");
                blocks.push(ContentBlock::Text {
                    text: format!("[Failed to download voice note: {e}]"),
                });
            }
        }
    }

    // Documents with image MIME types (e.g. uncompressed photos).
    if let Some(doc) = msg.document() {
        let is_image = doc.mime_type.as_ref()
            .map_or(false, |m| m.as_ref().starts_with("image/"));
        if is_image {
            tracing::info!(
                file_id = doc.file.id.0.as_str(),
                file_name = doc.file_name.as_deref().unwrap_or("unknown"),
                "downloading image document from Telegram"
            );
            let mime = doc.mime_type.as_ref()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "image/jpeg".to_string());

            match download_telegram_file(bot, &doc.file.id.0).await {
                Ok(data) => {
                    match media::resolve(media::MediaInput::Image {
                        data,
                        mime_type: mime,
                    }).await {
                        Ok(media::ResolvedMedia::Images(image_blocks)) => {
                            blocks.extend(image_blocks);
                        }
                        Ok(media::ResolvedMedia::Transcription(t)) => {
                            blocks.push(ContentBlock::Text { text: t });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to process document image");
                            unsupported.push(format!("{model} doesn't support vision"));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to download document");
                    blocks.push(ContentBlock::Text {
                        text: format!("[Failed to download document: {e}]"),
                    });
                }
            }
        }
    }

    // If somehow we have no blocks at all (shouldn't happen given the
    // earlier checks), return a fallback.
    if blocks.is_empty() && unsupported.is_empty() {
        blocks.push(ContentBlock::Text {
            text: "[Empty message received]".to_string(),
        });
    }

    Ok(ExtractedContent { blocks, unsupported })
}

/// Download a file from Telegram by file_id.
///
/// Uses the Bot API to get the file path, then downloads from the
/// Telegram file server.
async fn download_telegram_file(bot: &Bot, file_id: &str) -> crate::Result<Vec<u8>> {
    let file = bot.get_file(teloxide::types::FileId(file_id.to_string()))
        .await
        .map_err(|e| DysonError::Llm(format!("Telegram get_file failed: {e}")))?;

    let token = bot.token();
    let url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        token, file.path
    );

    let response = reqwest::get(&url)
        .await
        .map_err(|e| DysonError::Http(e))?;

    let bytes = response.bytes()
        .await
        .map_err(|e| DysonError::Http(e))?;

    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// TelegramOutput
// ---------------------------------------------------------------------------

/// Output implementation that sends agent responses to a Telegram chat.
///
/// Uses `tokio::task::block_in_place` + `Handle::block_on()` to bridge
/// the sync Output trait with async teloxide API calls.  This avoids the
/// "Cannot start a runtime from within a runtime" panic.
pub struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
    text_buffer: String,
    current_message_id: Option<MessageId>,
    last_edit: Instant,
    rt: tokio::runtime::Handle,
}

impl TelegramOutput {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        Self {
            bot,
            chat_id,
            text_buffer: String::new(),
            current_message_id: None,
            last_edit: Instant::now(),
            rt: tokio::runtime::Handle::current(),
        }
    }

    /// Bridge async teloxide call from sync context.
    ///
    /// Uses `block_in_place` to move off the async worker thread, then
    /// `block_on` to run the future.  This is the correct pattern for
    /// calling async code from sync code inside a tokio runtime.
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.rt.block_on(f))
    }

    fn send_message(&mut self, text: &str) -> Result<MessageId, DysonError> {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let text = text.to_string();

        let result = self.block_on(async {
            bot.send_message(chat_id, &text)
                .parse_mode(ParseMode::Html)
                .await
        });

        match result {
            Ok(msg) => Ok(msg.id),
            Err(e) => {
                tracing::error!(error = %e, "failed to send Telegram message");
                Err(DysonError::Llm(format!("Telegram send failed: {e}")))
            }
        }
    }

    fn edit_message(
        &self,
        message_id: MessageId,
        text: &str,
    ) -> Result<(), DysonError> {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let text = text.to_string();

        let result = self.block_on(async {
            bot.edit_message_text(chat_id, message_id, &text)
                .parse_mode(ParseMode::Html)
                .await
        });

        if let Err(e) = result {
            tracing::debug!(error = %e, "failed to edit Telegram message");
        }

        Ok(())
    }

    fn maybe_flush_text(&mut self) -> Result<(), DysonError> {
        if self.text_buffer.is_empty() {
            return Ok(());
        }

        let elapsed = self.last_edit.elapsed().as_millis();
        if elapsed < EDIT_INTERVAL_MS && self.current_message_id.is_some() {
            return Ok(());
        }

        let html = markdown_to_telegram_html(&self.text_buffer);
        let parts = split_for_telegram(&html);

        // While streaming, only edit/send the first chunk (the current message).
        // Full multi-message send happens in force_flush_text on completion.
        let text = &parts[0];

        match self.current_message_id {
            Some(msg_id) => self.edit_message(msg_id, text)?,
            None => {
                let msg_id = self.send_message(text)?;
                self.current_message_id = Some(msg_id);
            }
        }

        self.last_edit = Instant::now();
        Ok(())
    }

    fn force_flush_text(&mut self) -> Result<(), DysonError> {
        if self.text_buffer.is_empty() {
            return Ok(());
        }

        let html = markdown_to_telegram_html(&self.text_buffer);
        let parts = split_for_telegram(&html);

        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                // First chunk: edit the existing message or send a new one.
                match self.current_message_id {
                    Some(msg_id) => self.edit_message(msg_id, part)?,
                    None => {
                        let msg_id = self.send_message(part)?;
                        self.current_message_id = Some(msg_id);
                    }
                }
            } else {
                // Subsequent chunks: send as new messages.
                self.send_message(part)?;
            }
        }

        Ok(())
    }
}

impl Output for TelegramOutput {
    fn text_delta(&mut self, text: &str) -> Result<(), DysonError> {
        self.text_buffer.push_str(text);
        self.maybe_flush_text()?;
        Ok(())
    }

    fn tool_use_start(&mut self, _id: &str, _name: &str) -> Result<(), DysonError> {
        // Send "typing..." indicator so the user knows the agent is working.
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let _ = self.block_on(async {
            bot.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await
        });
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, _output: &ToolOutput) -> Result<(), DysonError> {
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<(), DysonError> {
        let bot = self.bot.clone();
        let chat_id = self.chat_id;
        let input_file = InputFile::file(path);

        let result = self.block_on(async {
            bot.send_document(chat_id, input_file).await
        });

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "failed to send file via Telegram");
                Err(DysonError::Llm(format!("Telegram send_document failed: {e}")))
            }
        }
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        let text = format!("❌ Error: {error}");
        self.send_message(&text)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.force_flush_text()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn split_for_telegram(text: &str) -> Vec<String> {
    if text.len() <= MAX_MESSAGE_LEN {
        return vec![text.to_string()];
    }

    let mut parts = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= MAX_MESSAGE_LEN {
            parts.push(remaining.to_string());
            break;
        }

        // Find a split point at MAX_MESSAGE_LEN, respecting UTF-8 boundaries.
        let mut end = MAX_MESSAGE_LEN;
        while !remaining.is_char_boundary(end) && end > 0 {
            end -= 1;
        }

        // Try to split at the last newline within the chunk for cleaner breaks.
        if let Some(nl) = remaining[..end].rfind('\n') {
            parts.push(remaining[..nl].to_string());
            remaining = &remaining[nl + 1..];
        } else {
            parts.push(remaining[..end].to_string());
            remaining = &remaining[end..];
        }
    }

    parts
}

/// Convert standard markdown to Telegram-compatible HTML.
///
/// Handles fenced code blocks, inline code, bold, italic, strikethrough,
/// links, headings, and blockquotes.  Text outside of code spans is
/// HTML-escaped so that `<`, `>`, and `&` don't break the parse.
///
/// Plain text without any markdown passes through unchanged (just escaped).
fn markdown_to_telegram_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // --- Fenced code blocks: ```lang ... ``` ---
        if line.trim_start().starts_with("```") {
            i += 1; // skip opening fence
            out.push_str("<pre>");
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    i += 1; // skip closing fence
                    break;
                }
                if !out.ends_with("<pre>") {
                    out.push('\n');
                }
                out.push_str(&escape_html(lines[i]));
                i += 1;
            }
            out.push_str("</pre>");
            out.push('\n');
            continue;
        }

        // --- Headings: # ... → <b>...</b> ---
        if let Some(rest) = strip_heading_prefix(line) {
            out.push_str("<b>");
            out.push_str(&convert_inline(&escape_html(rest)));
            out.push_str("</b>");
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Blockquote: > ... ---
        if let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix(">")) {
            out.push_str("<blockquote>");
            out.push_str(&convert_inline(&escape_html(rest)));
            out.push_str("</blockquote>");
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Horizontal rule: --- / *** / ___ ---
        let trimmed = line.trim();
        if trimmed.len() >= 3
            && (trimmed.chars().all(|c| c == '-')
                || trimmed.chars().all(|c| c == '*')
                || trimmed.chars().all(|c| c == '_'))
        {
            out.push('\n');
            i += 1;
            continue;
        }

        // --- Regular line: escape HTML, then convert inline markdown ---
        out.push_str(&convert_inline(&escape_html(line)));
        out.push('\n');
        i += 1;
    }

    // Remove trailing newline added by our line-by-line processing.
    if out.ends_with('\n') {
        out.pop();
    }

    out
}

/// Escape `&`, `<`, and `>` for Telegram HTML.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strip markdown heading prefix (`# `, `## `, etc.) and return the rest.
fn strip_heading_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let after_hashes = trimmed.trim_start_matches('#');
    // Must have at least one space after the hashes.
    after_hashes.strip_prefix(' ')
}

/// Convert inline markdown (bold, italic, strikethrough, code, links)
/// within an already HTML-escaped string.
///
/// Order matters: code spans first (so their contents aren't touched),
/// then bold before italic (since `**` contains `*`).
fn convert_inline(s: &str) -> String {
    let s = convert_inline_code(s);
    let s = convert_links(&s);
    let s = convert_pattern(&s, "**", "<b>", "</b>");
    let s = convert_pattern(&s, "__", "<b>", "</b>");
    let s = convert_pattern(&s, "~~", "<s>", "</s>");
    let s = convert_pattern(&s, "*", "<i>", "</i>");
    let s = convert_pattern(&s, "_", "<i>", "</i>");
    s
}

/// Convert `` `inline code` `` spans.
fn convert_inline_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('`') {
            out.push_str("<code>");
            out.push_str(&rest[..end]);
            out.push_str("</code>");
            rest = &rest[end + 1..];
        } else {
            // Unclosed backtick — keep literal.
            out.push('`');
        }
    }
    out.push_str(rest);
    out
}

/// Convert `[text](url)` markdown links to `<a href="url">text</a>`.
fn convert_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(bracket_start) = rest.find('[') {
        // Check this isn't an image link ![
        if bracket_start > 0 && rest.as_bytes()[bracket_start - 1] == b'!' {
            out.push_str(&rest[..bracket_start + 1]);
            rest = &rest[bracket_start + 1..];
            continue;
        }
        out.push_str(&rest[..bracket_start]);
        rest = &rest[bracket_start + 1..];

        if let Some(bracket_end) = rest.find(']') {
            let link_text = &rest[..bracket_end];
            let after_bracket = &rest[bracket_end + 1..];

            if after_bracket.starts_with('(') {
                if let Some(paren_end) = after_bracket.find(')') {
                    let url = &after_bracket[1..paren_end];
                    // Un-escape HTML entities in URL for the href attribute.
                    let raw_url = url
                        .replace("&amp;", "&")
                        .replace("&lt;", "<")
                        .replace("&gt;", ">");
                    out.push_str(&format!(
                        "<a href=\"{}\">{}</a>",
                        raw_url, link_text
                    ));
                    rest = &after_bracket[paren_end + 1..];
                    continue;
                }
            }

            // Not a valid link — emit the bracket and text literally.
            out.push('[');
            out.push_str(link_text);
            rest = &rest[bracket_end..];
        } else {
            out.push('[');
        }
    }
    out.push_str(rest);
    out
}

/// Convert a symmetric two-char or one-char markdown pattern to HTML tags.
///
/// `marker` is e.g. `"**"` or `"*"`.  Finds matched pairs and wraps them.
/// For single-char markers like `*` and `_`, avoids matching mid-word
/// underscores (e.g. `some_var_name` should not become italic).
fn convert_pattern(s: &str, marker: &str, open: &str, close: &str) -> String {
    let mlen = marker.len();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;

    while pos < s.len() {
        let rest = &s[pos..];

        // Skip content inside <code>, <pre>, <a> tags — don't convert
        // markdown inside already-converted spans.
        if rest.starts_with("<code>") {
            if let Some(end) = rest.find("</code>") {
                out.push_str(&rest[..end + 7]);
                pos += end + 7;
                continue;
            }
        }
        if rest.starts_with("<pre>") {
            if let Some(end) = rest.find("</pre>") {
                out.push_str(&rest[..end + 6]);
                pos += end + 6;
                continue;
            }
        }
        if rest.starts_with("<a ") {
            if let Some(end) = rest.find("</a>") {
                out.push_str(&rest[..end + 4]);
                pos += end + 4;
                continue;
            }
        }

        if rest.starts_with(marker) {
            // For single-char markers, don't match if preceded by alphanumeric
            // (avoids converting mid-word underscores).
            if mlen == 1 {
                let prev_char = s[..pos].chars().next_back();
                if prev_char.map_or(false, |c| c.is_ascii_alphanumeric()) {
                    out.push_str(marker);
                    pos += mlen;
                    continue;
                }
            }

            // Look for closing marker.
            let after_open = &s[pos + mlen..];
            if let Some(end_offset) = after_open.find(marker) {
                let inner = &after_open[..end_offset];
                let after = pos + mlen + end_offset + mlen;
                // For single-char markers, skip if closing marker is
                // followed by alphanumeric (mid-word).
                if mlen == 1 {
                    let next_char = s[after..].chars().next();
                    if next_char.map_or(false, |c| c.is_ascii_alphanumeric()) {
                        out.push_str(marker);
                        pos += mlen;
                        continue;
                    }
                }
                if !inner.is_empty() {
                    out.push_str(open);
                    out.push_str(inner);
                    out.push_str(close);
                    pos = after;
                    continue;
                }
            }
        }

        // Advance by one UTF-8 character.
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(
            markdown_to_telegram_html("Hello world"),
            "Hello world"
        );
    }

    #[test]
    fn plain_text_html_escaped() {
        assert_eq!(
            markdown_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn bold() {
        assert_eq!(
            markdown_to_telegram_html("this is **bold** text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn bold_underscore() {
        assert_eq!(
            markdown_to_telegram_html("this is __bold__ text"),
            "this is <b>bold</b> text"
        );
    }

    #[test]
    fn italic() {
        assert_eq!(
            markdown_to_telegram_html("this is *italic* text"),
            "this is <i>italic</i> text"
        );
    }

    #[test]
    fn strikethrough() {
        assert_eq!(
            markdown_to_telegram_html("this is ~~deleted~~ text"),
            "this is <s>deleted</s> text"
        );
    }

    #[test]
    fn inline_code() {
        assert_eq!(
            markdown_to_telegram_html("use `foo()` here"),
            "use <code>foo()</code> here"
        );
    }

    #[test]
    fn fenced_code_block() {
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        let expected = "before\n<pre>fn main() {}</pre>\nafter";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn fenced_code_block_escapes_html() {
        let input = "```\na < b\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre>a &lt; b</pre>"
        );
    }

    #[test]
    fn link() {
        assert_eq!(
            markdown_to_telegram_html("click [here](https://example.com)"),
            "click <a href=\"https://example.com\">here</a>"
        );
    }

    #[test]
    fn heading() {
        assert_eq!(
            markdown_to_telegram_html("# Title"),
            "<b>Title</b>"
        );
        assert_eq!(
            markdown_to_telegram_html("## Subtitle"),
            "<b>Subtitle</b>"
        );
        assert_eq!(
            markdown_to_telegram_html("### Deep"),
            "<b>Deep</b>"
        );
    }

    #[test]
    fn blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> quoted text"),
            "<blockquote>quoted text</blockquote>"
        );
    }

    #[test]
    fn horizontal_rule() {
        assert_eq!(markdown_to_telegram_html("---"), "");
        assert_eq!(markdown_to_telegram_html("***"), "");
        assert_eq!(markdown_to_telegram_html("___"), "");
    }

    #[test]
    fn combined_formatting() {
        assert_eq!(
            markdown_to_telegram_html("**bold** and *italic*"),
            "<b>bold</b> and <i>italic</i>"
        );
    }

    #[test]
    fn unclosed_backtick_kept() {
        assert_eq!(
            markdown_to_telegram_html("use `foo here"),
            "use `foo here"
        );
    }

    #[test]
    fn empty_string() {
        assert_eq!(markdown_to_telegram_html(""), "");
    }

    #[test]
    fn mid_word_underscores_preserved() {
        assert_eq!(
            markdown_to_telegram_html("some_var_name"),
            "some_var_name"
        );
    }

    #[test]
    fn code_content_not_formatted() {
        assert_eq!(
            markdown_to_telegram_html("`**not bold**`"),
            "<code>**not bold**</code>"
        );
    }

    #[test]
    fn multiline_message() {
        let input = "# Summary\n\nHello **world**.\n\n- item one\n- item two";
        let expected = "<b>Summary</b>\n\nHello <b>world</b>.\n\n- item one\n- item two";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn link_with_ampersand() {
        assert_eq!(
            markdown_to_telegram_html("[search](https://example.com?a=1&b=2)"),
            "<a href=\"https://example.com?a=1&b=2\">search</a>"
        );
    }

    #[test]
    fn multibyte_utf8_with_formatting() {
        // The exact crash case: en-dash (–) is 3 bytes, bold markers around it.
        assert_eq!(
            markdown_to_telegram_html("**pts/0** – your current shell"),
            "<b>pts/0</b> – your current shell"
        );
    }

    #[test]
    fn multibyte_utf8_emoji() {
        assert_eq!(
            markdown_to_telegram_html("hello **world** 🌍"),
            "hello <b>world</b> 🌍"
        );
    }
}
