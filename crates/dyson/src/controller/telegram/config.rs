//! Telegram transport configuration and validation.
use super::{TelegramController, api::BotApi};
use crate::config::{ControllerConfig, Settings};
use dyson_telegram::media::DownloadLimits;
use serde::Deserialize;
use std::sync::Arc;

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
#[derive(Debug, Deserialize)]
pub(super) struct TelegramControllerConfig {
    /// Bot API token (already resolved from secret reference by the config loader).
    #[serde(default)]
    pub(super) bot_token: Option<String>,
    /// Swarm-owned Telegram proxy config. Mutually exclusive with
    /// `bot_token`; dyson never sees the BotFather token in this mode.
    #[serde(default)]
    pub(super) proxy: Option<TelegramProxyConfig>,
    /// Update delivery mode. Standalone bots default to polling;
    /// swarm-managed bots run webhook mode.
    #[serde(default)]
    pub(super) mode: TelegramMode,
    /// Chat IDs allowed to interact.  Empty or absent = allow all.
    ///
    /// Accepts both numbers and strings (strings are parsed to i64).
    /// This is necessary because secret-resolved values become JSON
    /// strings — `{ "resolver": "insecure_env", "name": "MY_CHAT_ID" }`
    /// resolves to `"123456"` (a string), not `123456` (a number).
    #[serde(default, deserialize_with = "deserialize_chat_ids")]
    pub(super) allowed_chat_ids: Vec<i64>,
    /// Explicitly acknowledge that the bot accepts messages from any chat.
    /// Required when `allowed_chat_ids` is empty, to prevent accidental
    /// open access from config errors.
    #[serde(default)]
    pub(super) allow_all_chats: bool,
    /// Per-filetype download size limits.
    #[serde(default)]
    pub(super) download_limits: DownloadLimits,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TelegramProxyConfig {
    pub(super) base_url: String,
    pub(super) file_base_url: String,
    pub(super) bearer: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum TelegramMode {
    #[default]
    Polling,
    Webhook,
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

        match (&tg_config.bot_token, &tg_config.proxy) {
            (Some(_), Some(_)) => {
                tracing::error!(
                    "Telegram controller config must set either bot_token or proxy, not both"
                );
                return None;
            }
            (None, None) => {
                tracing::error!("Telegram controller config requires either bot_token or proxy");
                return None;
            }
            _ => {}
        }

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
            bot_token: tg_config.bot_token.map(crate::auth::Credential::new),
            proxy: tg_config.proxy,
            mode: tg_config.mode,
            allowed_chat_ids: tg_config.allowed_chat_ids,
            download_limits: Arc::new(tg_config.download_limits),
        })
    }

    pub(super) fn from_settings(settings: &Settings) -> Option<Self> {
        settings.controllers.iter().find_map(Self::from_config)
    }

    pub(super) fn build_bot(&self) -> BotApi {
        if let Some(proxy) = &self.proxy {
            return BotApi::new_with_base(
                proxy.base_url.clone(),
                Some(proxy.bearer.clone()),
                proxy.file_base_url.clone(),
            );
        }
        let token = self
            .bot_token
            .as_ref()
            .expect("telegram config validation requires bot_token or proxy");
        BotApi::new(token.expose())
    }
}
