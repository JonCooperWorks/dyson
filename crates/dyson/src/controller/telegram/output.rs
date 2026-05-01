// ===========================================================================
// TelegramOutput — bridges the sync Output trait with the async Telegram API.
//
// Uses `tokio::task::block_in_place` + `Handle::block_on()` to bridge
// the sync Output trait with async API calls.
// ===========================================================================

use std::path::Path;
use std::time::Instant;

use crate::controller::Output;
use crate::error::{DysonError, LlmErrorKind, LlmRecovery, classify_llm_error};
use crate::tool::ToolOutput;

use super::EDIT_INTERVAL_MS;
use super::api::BotApi;
use super::formatting::{escape_html, markdown_to_telegram_html, split_for_telegram};
use super::types::{ChatId, MessageId};

/// Output implementation that sends agent responses to a Telegram chat.
///
/// Uses `tokio::task::block_in_place` + `Handle::block_on()` to bridge
/// the sync Output trait with async Telegram API calls.
pub struct TelegramOutput {
    bot: BotApi,
    chat_id: ChatId,
    text_buffer: String,
    current_message_id: Option<MessageId>,
    last_edit: Instant,
    rt: tokio::runtime::Handle,
    has_text: bool,
    typing_handle: Option<tokio::task::JoinHandle<()>>,
    /// All Telegram message IDs sent during this output session.
    /// Used to map reactions back to conversation turns.
    sent_message_ids: Vec<MessageId>,
    /// Number of parts (from `split_for_telegram`) already finalized as
    /// their own Telegram messages.  Once a part is finalized, it won't
    /// be edited again — the next streaming update goes into a fresh
    /// message for `parts[committed_count]`.
    committed_count: usize,
}

impl TelegramOutput {
    pub fn new(bot: BotApi, chat_id: ChatId, has_text: bool) -> Self {
        Self {
            bot,
            chat_id,
            text_buffer: String::new(),
            current_message_id: None,
            has_text,
            last_edit: Instant::now(),
            rt: tokio::runtime::Handle::current(),
            typing_handle: None,
            sent_message_ids: Vec::new(),
            committed_count: 0,
        }
    }

    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.rt.block_on(f))
    }

    pub fn send_message(&mut self, text: &str) -> Result<MessageId, DysonError> {
        let result = self.block_on(self.bot.send_message_html(self.chat_id, text));
        match result {
            Ok(msg) => {
                let id = msg.id();
                self.sent_message_ids.push(id);
                Ok(id)
            }
            Err(e) if is_telegram_parse_error(&e) => {
                tracing::warn!(error = %e, "HTML parse failed, falling back to plain text");
                let plain = strip_html_tags(text);
                let fallback = self.block_on(self.bot.send_message(self.chat_id, &plain));
                match fallback {
                    Ok(msg) => {
                        let id = msg.id();
                        self.sent_message_ids.push(id);
                        Ok(id)
                    }
                    Err(e2) => {
                        tracing::error!(error = %e2, "plain-text fallback also failed");
                        Err(e2)
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to send Telegram message");
                Err(e)
            }
        }
    }

    /// Returns all Telegram message IDs sent during this output session.
    pub fn sent_message_ids(&self) -> &[MessageId] {
        &self.sent_message_ids
    }

    fn edit_message(&self, message_id: MessageId, text: &str) {
        let result = self.block_on(self.bot.edit_message_text(self.chat_id, message_id, text));
        if let Err(e) = result {
            if is_telegram_parse_error(&e) {
                tracing::warn!(error = %e, "HTML parse failed on edit, falling back to plain text");
                let plain = strip_html_tags(text);
                let _ = self.block_on(self.bot.edit_message_text_plain(
                    self.chat_id,
                    message_id,
                    &plain,
                ));
            } else {
                tracing::debug!(error = %e, "failed to edit Telegram message");
            }
        }
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

        // Once the buffer crosses `MAX_MESSAGE_LEN`, the content of
        // `parts[0..parts.len() - 1]` is stable — appending more text only
        // extends the final part.  So we can safely finalize any newly
        // stabilized parts mid-stream and advance `current_message_id` to
        // a fresh message for the still-growing tail.  This means splits
        // actually happen as soon as the overflow occurs, instead of
        // being deferred entirely to `force_flush_text`.
        let last_idx = parts.len() - 1;
        while self.committed_count < last_idx {
            self.write_part(&parts[self.committed_count])?;
            self.committed_count += 1;
            // Next part begins a new message.
            self.current_message_id = None;
        }

        // Show the still-growing tail in the current (non-finalized) message.
        self.write_part(&parts[self.committed_count])?;

        self.last_edit = Instant::now();
        Ok(())
    }

    fn force_flush_text(&mut self) -> Result<(), DysonError> {
        if self.text_buffer.is_empty() {
            return Ok(());
        }

        let html = markdown_to_telegram_html(&self.text_buffer);
        let parts = split_for_telegram(&html);

        // Finalize every remaining part, including the tail that was
        // previously treated as still-growing.
        while self.committed_count < parts.len() {
            self.write_part(&parts[self.committed_count])?;
            self.committed_count += 1;
            self.current_message_id = None;
        }

        // Reset state so a subsequent `flush()` on the same output is a
        // no-op instead of re-sending everything.
        self.text_buffer.clear();
        self.committed_count = 0;
        self.current_message_id = None;

        Ok(())
    }

    /// Render `part` into the current message — editing if one already
    /// exists for this part, otherwise sending a new message.  Empty
    /// parts are skipped (Telegram rejects empty text).
    fn write_part(&mut self, part: &str) -> Result<(), DysonError> {
        if part.is_empty() {
            return Ok(());
        }
        match self.current_message_id {
            Some(msg_id) => self.edit_message(msg_id, part),
            None => {
                let msg_id = self.send_message(part)?;
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
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<(), DysonError> {
        Ok(())
    }

    fn tool_result(&mut self, _output: &ToolOutput) -> Result<(), DysonError> {
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<(), DysonError> {
        let result = self.block_on(self.bot.send_document(self.chat_id, path));

        // Mirror the file into the HTTP controller's artefact store so
        // the web UI's Artefacts tab shows it for the same chat id.
        // No-op when no HTTP controller is running in this process.
        // Done after the Telegram send returns so a slow browser can't
        // delay document delivery to the user.
        if let Some(sink) = crate::controller::browser_artefact_sink() {
            let chat_key = self.chat_id.0.to_string();
            sink.publish_file_as_artefact(&chat_key, path);
        }

        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "failed to send file via Telegram");
                Err(e)
            }
        }
    }

    fn send_artefact(&mut self, artefact: &crate::message::Artefact) -> Result<(), DysonError> {
        // Telegram has no inline-markdown surface, so dump the report
        // to a throwaway temp file and send it as a document — the user
        // gets the same .md they'd download from the web UI.  We flush
        // any buffered text first so the report doesn't arrive ahead
        // of the narrative it belongs to.
        self.force_flush_text()?;

        let suffix = match artefact.mime_type.as_str() {
            "text/markdown" => "md",
            _ => "txt",
        };
        let mut safe_title: String = artefact
            .title
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if safe_title.is_empty() {
            safe_title.push_str("artefact");
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!("dyson-{safe_title}-{nanos}.{suffix}"));
        if let Err(e) = std::fs::write(&path, &artefact.content) {
            tracing::error!(error = %e, "failed to write artefact to temp file");
            return Err(DysonError::Io(e));
        }
        let result = self.block_on(self.bot.send_document(self.chat_id, &path));
        // Best-effort cleanup; ignore failure.
        let _ = std::fs::remove_file(&path);
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    title = %artefact.title,
                    "failed to send artefact via Telegram",
                );
                Err(e)
            }
        }
    }

    fn error(&mut self, error: &DysonError) -> Result<(), DysonError> {
        let text = format!("Error: {error}");
        self.send_message(&text)?;
        Ok(())
    }

    fn on_llm_error(&mut self, error: &DysonError) -> LlmRecovery {
        match classify_llm_error(&error.to_string()) {
            LlmErrorKind::NoToolUse => {
                let _ =
                    self.send_message("Model doesn't support tool use — retrying without tools.");
                LlmRecovery::RetryWithoutTools
            }
            LlmErrorKind::NoVision if self.has_text => {
                let _ =
                    self.send_message("Model doesn't support vision — retrying with text only.");
                LlmRecovery::RetryWithoutImages
            }
            LlmErrorKind::NoVision => {
                let _ = self.send_message("Model doesn't support vision.");
                let escaped = escape_html(&error.to_string());
                let _ = self.send_message(&format!("<pre>{escaped}</pre>"));
                LlmRecovery::GiveUp
            }
            LlmErrorKind::Other => {
                let escaped = escape_html(&error.to_string());
                let _ = self.send_message(&format!("Error:\n<pre>{escaped}</pre>"));
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
                    let _ = bot.send_typing(chat_id).await;
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

/// Returns `true` if the error is a Telegram "can't parse entities" rejection.
fn is_telegram_parse_error(e: &DysonError) -> bool {
    let msg = e.to_string();
    msg.contains("can't parse entities")
}

/// Strip HTML tags from a string, producing readable plain text.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_telegram_parse_error() {
        let err = DysonError::Llm(
            "Telegram sendMessage failed: Bad Request: can't parse entities: \
             Unexpected end tag at byte offset 1116"
                .to_string(),
        );
        assert!(is_telegram_parse_error(&err));
    }

    #[test]
    fn non_parse_error_not_detected() {
        let err = DysonError::Llm(
            "Telegram sendMessage failed: Bad Request: text must be non-empty".to_string(),
        );
        assert!(!is_telegram_parse_error(&err));
    }

    #[test]
    fn strip_tags_basic() {
        assert_eq!(
            strip_html_tags("<b>bold</b> and <i>italic</i>"),
            "bold and italic"
        );
    }

    #[test]
    fn strip_tags_pre_block() {
        assert_eq!(strip_html_tags("<pre>fn main() {}</pre>"), "fn main() {}");
    }

    #[test]
    fn strip_tags_no_tags() {
        assert_eq!(strip_html_tags("plain text"), "plain text");
    }

    #[test]
    fn strip_tags_preserves_entities() {
        assert_eq!(
            strip_html_tags("a &lt; b &amp; c &gt; d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn strip_tags_nested() {
        assert_eq!(
            strip_html_tags("<b>bold <i>and italic</i></b>"),
            "bold and italic"
        );
    }

    #[test]
    fn strip_tags_with_attributes() {
        assert_eq!(
            strip_html_tags("<a href=\"https://example.com\">link</a>"),
            "link"
        );
    }
}
