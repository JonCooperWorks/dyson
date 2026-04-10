// ===========================================================================
// Minimal Telegram Bot API types — just enough for Dyson's usage.
//
// Replaces the teloxide dependency with ~150 lines of serde structs.
// All fields use Option<> or #[serde(default)] to tolerate unknown fields.
// ===========================================================================

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core identifiers
// ---------------------------------------------------------------------------

/// Telegram chat identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatId(pub i64);

/// Telegram message identifier (unique within a chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(pub i32);

// ---------------------------------------------------------------------------
// Updates (long-polling response)
// ---------------------------------------------------------------------------

/// A single update from getUpdates.
#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
    #[serde(default)]
    pub message_reaction: Option<MessageReactionUpdated>,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// A Telegram message.
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i32,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub entities: Option<Vec<MessageEntity>>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub document: Option<Document>,
}

impl Message {
    pub fn id(&self) -> MessageId {
        MessageId(self.message_id)
    }
}

/// Telegram chat type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    #[default]
    Private,
    Group,
    Supergroup,
    Channel,
}

/// A Telegram chat.
#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub chat_type: ChatType,
}

impl Chat {
    /// Returns true if this is a group or supergroup chat.
    pub fn is_group(&self) -> bool {
        matches!(self.chat_type, ChatType::Group | ChatType::Supergroup)
    }
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// A Telegram user or bot.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub username: Option<String>,
}

// ---------------------------------------------------------------------------
// Message entities (mentions, commands, etc.)
// ---------------------------------------------------------------------------

/// A single entity within a message (mention, command, URL, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub offset: usize,
    pub length: usize,
}

// ---------------------------------------------------------------------------
// Media types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Callback queries (inline keyboard)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub message: Option<Message>,
}

// ---------------------------------------------------------------------------
// File download
// ---------------------------------------------------------------------------

/// Response from getFile — contains the path for downloading.
#[derive(Debug, Deserialize)]
pub struct File {
    pub file_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
    /// File size in bytes (provided by Telegram, used for early rejection).
    #[serde(default)]
    pub file_size: Option<u64>,
}

// ---------------------------------------------------------------------------
// Inline keyboard
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    pub fn new(rows: Vec<Vec<InlineKeyboardButton>>) -> Self {
        Self {
            inline_keyboard: rows,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}

impl InlineKeyboardButton {
    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(data.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Message reactions (Bot API 7.0+)
// ---------------------------------------------------------------------------

/// A reaction change on a message.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageReactionUpdated {
    pub chat: Chat,
    pub message_id: i32,
    #[serde(default)]
    pub user: Option<User>,
    /// The new list of reactions set by the user (empty = reaction removed).
    #[serde(default)]
    pub new_reaction: Vec<ReactionType>,
}

/// A single reaction emoji (standard or custom).
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionType {
    /// "emoji" for standard Unicode reactions, "custom_emoji" for premium.
    #[serde(rename = "type")]
    pub reaction_type: String,
    /// The emoji string (present when `reaction_type` is "emoji").
    #[serde(default)]
    pub emoji: Option<String>,
}

// ---------------------------------------------------------------------------
// API response wrapper
// ---------------------------------------------------------------------------

/// Generic Telegram Bot API response.
#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    #[serde(default)]
    pub description: Option<String>,
}
