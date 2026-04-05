// ===========================================================================
// TelegramOutput — bridges the sync Output trait with async teloxide API.
// ===========================================================================

use std::path::Path;
use std::time::Instant;

use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, MessageId, ParseMode};

use crate::controller::Output;
use crate::error::{classify_llm_error, DysonError, LlmErrorKind, LlmRecovery};
use crate::tool::ToolOutput;

use super::formatting::{escape_html, markdown_to_telegram_html, split_for_telegram};
use super::EDIT_INTERVAL_MS;

/// Output implementation that sends agent responses to a Telegram chat.
///
/// Uses `tokio::task::block_in_place` + `Handle::block_on()` to bridge
/// the sync Output trait with async teloxide API calls.
pub struct TelegramOutput {
    bot: Bot,
    chat_id: ChatId,
    text_buffer: String,
    current_message_id: Option<MessageId>,
    last_edit: Instant,
    rt: tokio::runtime::Handle,
    has_text: bool,
    typing_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TelegramOutput {
    pub fn new(bot: Bot, chat_id: ChatId, has_text: bool) -> Self {
        Self {
            bot,
            chat_id,
            text_buffer: String::new(),
            current_message_id: None,
            has_text,
            last_edit: Instant::now(),
            rt: tokio::runtime::Handle::current(),
            typing_handle: None,
        }
    }

    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.rt.block_on(f))
    }

    pub fn send_message(&mut self, text: &str) -> Result<MessageId, DysonError> {
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

    fn edit_message(&self, message_id: MessageId, text: &str) -> Result<(), DysonError> {
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
                match self.current_message_id {
                    Some(msg_id) => self.edit_message(msg_id, part)?,
                    None => {
                        let msg_id = self.send_message(part)?;
                        self.current_message_id = Some(msg_id);
                    }
                }
            } else {
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

        let result = self.block_on(async { bot.send_document(chat_id, input_file).await });

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "failed to send file via Telegram");
                Err(DysonError::Llm(format!(
                    "Telegram send_document failed: {e}"
                )))
            }
        }
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        let text = format!("❌ Error: {error}");
        self.send_message(&text)?;
        Ok(())
    }

    fn on_llm_error(&mut self, error: &DysonError) -> LlmRecovery {
        match classify_llm_error(&error.to_string()) {
            LlmErrorKind::NoToolUse => {
                let _ = self
                    .send_message("⚠️ Model doesn't support tool use — retrying without tools.");
                LlmRecovery::RetryWithoutTools
            }
            LlmErrorKind::NoVision if self.has_text => {
                let _ = self
                    .send_message("⚠️ Model doesn't support vision — retrying with text only.");
                LlmRecovery::RetryWithoutImages
            }
            LlmErrorKind::NoVision => {
                let _ = self.send_message("⚠️ Model doesn't support vision.");
                let escaped = escape_html(&error.to_string());
                let _ = self.send_message(&format!("<pre>{escaped}</pre>"));
                LlmRecovery::GiveUp
            }
            LlmErrorKind::Other => {
                let escaped = escape_html(&error.to_string());
                let _ = self.send_message(&format!("❌ Error:\n<pre>{escaped}</pre>"));
                LlmRecovery::GiveUp
            }
        }
    }

    fn typing_indicator(&mut self, visible: bool) -> Result<(), DysonError> {
        if visible {
            if self.typing_handle.is_some() {
                return Ok(());
            }
            let bot = self.bot.clone();
            let chat_id = self.chat_id;
            self.typing_handle = Some(tokio::spawn(async move {
                loop {
                    let _ = bot
                        .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                        .await;
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                }
            }));
        } else if let Some(handle) = self.typing_handle.take() {
            handle.abort();
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DysonError> {
        self.force_flush_text()?;
        Ok(())
    }
}

impl Drop for TelegramOutput {
    fn drop(&mut self) {
        if let Some(handle) = self.typing_handle.take() {
            handle.abort();
        }
    }
}
