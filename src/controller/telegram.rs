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
use std::sync::Arc;
use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::Mutex;

use serde::Deserialize;

use crate::config::{ControllerConfig, Settings};
use crate::controller::Output;
use crate::error::DysonError;
use crate::tool::ToolOutput;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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
             - Do NOT use markdown formatting. Telegram's markdown parser is strict and will \
               break on unescaped characters. Use plain text only.\n\
             - Keep responses concise. Telegram messages have a 4096 character limit.\n\
             - Use line breaks for readability instead of headers or bullet formatting.\n\
             - When showing code, use plain indentation, not fenced code blocks."
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
        let agents: Arc<Mutex<HashMap<i64, crate::agent::Agent>>> =
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

                // Extract text message.
                let msg = match &update.kind {
                    teloxide::types::UpdateKind::Message(m) => m.clone(),
                    _ => continue,
                };

                let text = match msg.text() {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => continue,
                };

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
                // The old history is preserved as a timestamped file
                // for review or RAG indexing.
                if text == "/clear" {
                    agents.lock().await.remove(&chat_id.0);
                    let _ = chat_store.rotate(&chat_id.0.to_string());
                    let _ = bot.send_message(chat_id, "Context cleared.").await;
                    tracing::info!(chat_id = chat_id.0, "conversation rotated and cleared");
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

                // /models — list available providers.
                if text == "/models" {
                    let providers = super::list_providers(&current_settings);
                    if providers.is_empty() {
                        let _ = bot.send_message(chat_id, "No providers configured.").await;
                    } else {
                        let mut reply = String::from("Available providers:\n");
                        for (name, pc) in &providers {
                            reply.push_str(&format!(
                                "  {} — {:?} ({})\n",
                                name, pc.provider_type, pc.model,
                            ));
                        }
                        let _ = bot.send_message(chat_id, reply).await;
                    }
                    continue;
                }

                // /model <name> — switch to a named provider.
                if let Some(name) = text.strip_prefix("/model ").map(str::trim) {
                    if name.is_empty() {
                        let _ = bot.send_message(chat_id, "Usage: /model <provider-name>").await;
                        continue;
                    }
                    let existing_messages = {
                        let agents_map = agents.lock().await;
                        agents_map
                            .get(&chat_id.0)
                            .map(|a| a.messages().to_vec())
                            .unwrap_or_default()
                    };
                    match super::build_agent_with_provider(
                        &current_settings,
                        name,
                        controller_prompt.as_deref(),
                        existing_messages,
                    )
                    .await
                    {
                        Ok(new_agent) => {
                            let pc = &current_settings.providers[name];
                            let reply = format!(
                                "Switched to '{}' — {:?} ({})",
                                name, pc.provider_type, pc.model,
                            );
                            agents.lock().await.insert(chat_id.0, new_agent);
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
                        .send_message(chat_id, "Usage: /model <provider-name>")
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

                    // Get or create the per-chat agent.
                    let mut agents_map = agents_clone.lock().await;
                    if let std::collections::hash_map::Entry::Vacant(entry) = agents_map.entry(chat_id.0) {
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
                                entry.insert(agent);
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "failed to create agent");
                                let _ = bot_clone.send_message(chat_id, format!("Error: {e}")).await;
                                return;
                            }
                        }
                    }
                    let agent = agents_map.get_mut(&chat_id.0)
                        .expect("agent must exist — just inserted above");

                    let mut output = TelegramOutput::new(bot_clone.clone(), chat_id);

                    if let Err(e) = agent.run(&text, &mut output).await {
                        tracing::error!(error = %e, "agent run failed");
                        let _ = bot_clone.send_message(chat_id, format!("Error: {e}")).await;
                    }

                    // Persist conversation history to disk after each turn.
                    if let Err(e) = store_clone.save(&chat_key, agent.messages()) {
                        tracing::error!(error = %e, "failed to save chat history");
                    }

                    drop(agents_map);
                });
            }
        }

        Ok(())
    }
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
            bot.send_message(chat_id, &text).await
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
            bot.edit_message_text(chat_id, message_id, &text).await
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

        let text = truncate_for_telegram(&self.text_buffer);

        match self.current_message_id {
            Some(msg_id) => self.edit_message(msg_id, &text)?,
            None => {
                let msg_id = self.send_message(&text)?;
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

        let text = truncate_for_telegram(&self.text_buffer);

        match self.current_message_id {
            Some(msg_id) => self.edit_message(msg_id, &text)?,
            None => {
                let msg_id = self.send_message(&text)?;
                self.current_message_id = Some(msg_id);
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

fn truncate_for_telegram(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_LEN {
        return text.to_string();
    }
    let mut end = MAX_MESSAGE_LEN;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}… (truncated)", &text[..end])
}
