// ===========================================================================
// Thin Telegram Bot API client — replaces teloxide.
//
// Reuses the process-wide reqwest::Client from http::client() so there's
// no duplicate TLS state or connection pool.  Each method is a single
// HTTP POST to the Bot API.
// ===========================================================================

use std::path::Path;

use serde_json::json;

use crate::error::DysonError;

use super::types::*;

/// Lightweight Telegram Bot API client.
#[derive(Clone)]
pub struct BotApi {
    token: String,
    base_url: String,
}

impl BotApi {
    /// Create a new client with the given bot token.
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        let base_url = format!("https://api.telegram.org/bot{token}");
        Self { token, base_url }
    }

    /// The bot token (needed for file download URLs).
    pub fn token(&self) -> &str {
        &self.token
    }

    // -----------------------------------------------------------------------
    // API methods
    // -----------------------------------------------------------------------

    /// Fetch the bot's own user info (username, id, etc.).
    pub async fn get_me(&self) -> Result<User, DysonError> {
        let body = serde_json::json!({});
        self.post_result("getMe", &body).await
    }

    /// Long-poll for updates.
    ///
    /// Requests message, callback_query, and message_reaction update types
    /// so we can capture emoji feedback on bot responses.
    pub async fn get_updates(
        &self,
        offset: i64,
        timeout: u64,
    ) -> Result<Vec<Update>, DysonError> {
        let body = json!({
            "offset": offset,
            "timeout": timeout,
            "allowed_updates": ["message", "callback_query", "message_reaction"],
        });
        let resp: ApiResponse<Vec<Update>> = self.post("getUpdates", &body).await?;
        Ok(resp.result.unwrap_or_default())
    }

    /// Send a text message.
    pub async fn send_message(
        &self,
        chat_id: ChatId,
        text: &str,
    ) -> Result<Message, DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "text": text,
        });
        self.post_result("sendMessage", &body).await
    }

    /// Send a text message with HTML parse mode.
    pub async fn send_message_html(
        &self,
        chat_id: ChatId,
        text: &str,
    ) -> Result<Message, DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "text": text,
            "parse_mode": "HTML",
        });
        self.post_result("sendMessage", &body).await
    }

    /// Send a text message with an inline keyboard.
    pub async fn send_message_with_keyboard(
        &self,
        chat_id: ChatId,
        text: &str,
        keyboard: &InlineKeyboardMarkup,
    ) -> Result<Message, DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "text": text,
            "reply_markup": keyboard,
        });
        self.post_result("sendMessage", &body).await
    }

    /// Edit an existing message's text (HTML mode).
    ///
    /// Returns an error if the Bot API rejects the edit (for example,
    /// "can't parse entities" when the HTML split cut mid-tag).  The
    /// benign `message is not modified` response is swallowed so callers
    /// can retry edits idempotently.
    pub async fn edit_message_text(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: &str,
    ) -> Result<(), DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "message_id": message_id.0,
            "text": text,
            "parse_mode": "HTML",
        });
        let resp: ApiResponse<serde_json::Value> = self.post("editMessageText", &body).await?;
        check_edit_ok(resp, "editMessageText")
    }

    /// Edit an existing message's text (plain text, no parse mode).
    pub async fn edit_message_text_plain(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: &str,
    ) -> Result<(), DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "message_id": message_id.0,
            "text": text,
        });
        let resp: ApiResponse<serde_json::Value> = self.post("editMessageText", &body).await?;
        check_edit_ok(resp, "editMessageText")
    }

    /// Send a document (file) by path.
    pub async fn send_document(
        &self,
        chat_id: ChatId,
        path: &Path,
    ) -> Result<Message, DysonError> {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());

        let file_bytes = tokio::fs::read(path).await.map_err(DysonError::Io)?;

        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .unwrap();

        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.0.to_string())
            .part("document", part);

        let resp = crate::http::client()
            .post(format!("{}/sendDocument", self.base_url))
            .multipart(form)
            .send()
            .await
            .map_err(DysonError::Http)?;

        let api_resp: ApiResponse<Message> = resp.json().await.map_err(DysonError::Http)?;
        api_resp
            .result
            .ok_or_else(|| DysonError::Llm(format!(
                "sendDocument failed: {}",
                api_resp.description.unwrap_or_default()
            )))
    }

    /// Answer a callback query (dismiss the loading spinner on inline buttons).
    pub async fn answer_callback_query(&self, callback_query_id: &str) -> Result<(), DysonError> {
        let body = json!({
            "callback_query_id": callback_query_id,
        });
        let _: ApiResponse<bool> = self.post("answerCallbackQuery", &body).await?;
        Ok(())
    }

    /// Get file metadata (including download path).
    pub async fn get_file(&self, file_id: &str) -> Result<File, DysonError> {
        let body = json!({ "file_id": file_id });
        self.post_result("getFile", &body).await
    }

    /// Send a chat action (e.g. "typing").
    pub async fn send_typing(&self, chat_id: ChatId) -> Result<(), DysonError> {
        let body = json!({
            "chat_id": chat_id.0,
            "action": "typing",
        });
        let _: ApiResponse<bool> = self.post("sendChatAction", &body).await?;
        Ok(())
    }

    /// Download a file by its file_id, enforcing a size limit.
    ///
    /// Calls getFile to get the path and optional file size.  If the
    /// server-reported size exceeds `max_bytes`, rejects immediately.
    /// Otherwise streams the download, checking size incrementally so
    /// we never buffer more than `max_bytes` in memory.
    pub async fn download_file(
        &self,
        file_id: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, DysonError> {
        let file = self.get_file(file_id).await?;

        // Early reject based on Telegram's reported file size.
        if let Some(size) = file.file_size
            && size > max_bytes
        {
            return Err(DysonError::Llm(format!(
                "Telegram file too large ({size} bytes, limit {max_bytes})"
            )));
        }

        let file_path = file.file_path.ok_or_else(|| {
            DysonError::Llm("Telegram getFile returned no file_path".to_string())
        })?;

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.token, file_path
        );

        let mut response = crate::http::client()
            .get(&url)
            .send()
            .await
            .map_err(DysonError::Http)?;

        // Stream the body in chunks, enforcing the limit incrementally.
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(DysonError::Http)? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > max_bytes {
                return Err(DysonError::Llm(format!(
                    "Telegram file download exceeded limit ({max_bytes} bytes)"
                )));
            }
        }
        Ok(bytes)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// POST a JSON body to a Bot API method and deserialize the response.
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<T, DysonError> {
        let resp = crate::http::client()
            .post(format!("{}/{method}", self.base_url))
            .json(body)
            .send()
            .await
            .map_err(DysonError::Http)?;

        resp.json::<T>().await.map_err(DysonError::Http)
    }

    /// POST and unwrap the `result` field, returning an error if `ok` is false.
    async fn post_result<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<T, DysonError> {
        let resp: ApiResponse<T> = self.post(method, body).await?;
        resp.result.ok_or_else(|| {
            DysonError::Llm(format!(
                "Telegram {method} failed: {}",
                resp.description.unwrap_or_default()
            ))
        })
    }
}

/// Validate an `editMessageText` response.
///
/// The benign `message is not modified` error is swallowed — it just means
/// the text we sent was identical to the current message content.  Any other
/// failure (parse errors, length errors, etc.) becomes a `DysonError::Llm`
/// so callers can fall back to an alternate render (e.g. plain text).
fn check_edit_ok(resp: ApiResponse<serde_json::Value>, method: &str) -> Result<(), DysonError> {
    if resp.result.is_some() {
        return Ok(());
    }
    let description = resp.description.unwrap_or_default();
    if description.contains("message is not modified") {
        return Ok(());
    }
    Err(DysonError::Llm(format!(
        "Telegram {method} failed: {description}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_edit_ok_success_passes() {
        let resp = ApiResponse {
            ok: true,
            result: Some(serde_json::json!({"message_id": 1})),
            description: None,
        };
        assert!(check_edit_ok(resp, "editMessageText").is_ok());
    }

    #[test]
    fn check_edit_ok_not_modified_is_benign() {
        let resp = ApiResponse::<serde_json::Value> {
            ok: false,
            result: None,
            description: Some("Bad Request: message is not modified".to_string()),
        };
        assert!(check_edit_ok(resp, "editMessageText").is_ok());
    }

    #[test]
    fn check_edit_ok_parse_entities_propagates() {
        let resp = ApiResponse::<serde_json::Value> {
            ok: false,
            result: None,
            description: Some(
                "Bad Request: can't parse entities: Unexpected end tag".to_string(),
            ),
        };
        let err = check_edit_ok(resp, "editMessageText").unwrap_err();
        assert!(err.to_string().contains("can't parse entities"));
    }
}
