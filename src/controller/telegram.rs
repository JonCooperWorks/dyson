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
//     │           ├── output.tool_use_start(...)  → send "🔧 bash"
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

use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId};

use serde::Deserialize;

use crate::agent::Agent;
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
    bot_token: String,
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
                        config = %config.config,
                        "failed to parse telegram controller config — is bot_token set?"
                    );
                    return None;
                }
            };

        Some(Self {
            bot_token: tg_config.bot_token,
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

        let bot = teloxide::Bot::new(&self.bot_token);
        let allowed_ids = self.allowed_chat_ids.clone();

        // Clone full settings for skill creation inside the closure.
        let mut settings_inner = settings.clone();

        // Append the controller's system prompt to the agent settings.
        if let Some(prompt) = self.system_prompt() {
            settings_inner.agent.system_prompt.push_str("\n\n");
            settings_inner.agent.system_prompt.push_str(prompt);
        }

        let agent_settings = settings_inner.agent.clone();

        teloxide::repl(bot, move |msg: Message, bot: Bot| {
            let agent_settings = agent_settings.clone();
            let settings_inner = settings_inner.clone();
            let allowed_ids = allowed_ids.clone();

            async move {
                let text = match msg.text() {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => return Ok(()),
                };

                let chat_id = msg.chat.id;

                // /whoami — reply with the chat ID so users can
                // bootstrap their allowed_chat_ids config.
                // This runs BEFORE access control so you can always
                // discover your chat ID, even on a locked-down bot.
                if text == "/whoami" {
                    let _ = bot.send_message(chat_id, format!("Your chat ID: `{}`", chat_id.0)).await;
                    return Ok(());
                }

                // Access control.
                if !allowed_ids.is_empty() && !allowed_ids.contains(&chat_id.0) {
                    tracing::warn!(chat_id = chat_id.0, "unauthorized chat — ignoring");
                    return Ok(());
                }

                tracing::info!(chat_id = chat_id.0, "telegram message received");

                // Build a fresh agent per message.
                let client = crate::llm::create_client(&agent_settings);
                let sandbox = crate::sandbox::create_sandbox(
                    &settings_inner.sandbox,
                    settings_inner.dangerous_no_sandbox,
                );
                let skills = crate::skill::create_skills(&settings_inner).await;

                let mut agent = match Agent::new(client, sandbox, skills, &agent_settings) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to create agent");
                        return Ok(());
                    }
                };

                let mut output = TelegramOutput::new(bot.clone(), chat_id);

                if let Err(e) = agent.run(&text, &mut output).await {
                    tracing::error!(error = %e, "agent run failed");
                    let _ = bot.send_message(chat_id, format!("Error: {e}")).await;
                }

                Ok(())
            }
        })
        .await;

        Ok(())
    }
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

    fn tool_use_start(&mut self, _id: &str, name: &str) -> Result<(), DysonError> {
        self.force_flush_text()?;
        self.text_buffer.clear();
        self.current_message_id = None;
        self.send_message(&format!("🔧 {name}"))?;
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<(), DysonError> {
        let label = if output.is_error {
            "❌ Error"
        } else {
            "✅ Result"
        };
        let content = truncate_for_telegram(&output.content);
        let text = format!("{label}\n```\n{content}\n```");
        self.send_message(&text)?;
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
