// ===========================================================================
// HTTP controller — wire DTOs.
//
// Public types serialised to JSON over the API and SSE channel.  Stable
// shape: the SPA in `web/` and any external clients (the in-tree CLI
// load-tests; user scripts) deserialise these — keep field names and
// variants additive.
// ===========================================================================

use serde::{Deserialize, Serialize};

use crate::tool::view::ToolView;

#[derive(Serialize)]
pub(crate) struct ConversationDto {
    pub(crate) id: String,
    pub(crate) title: String,
    /// `true` while a turn is currently executing for this chat.
    pub(crate) live: bool,
    /// `true` when at least one artefact has ever been emitted for
    /// this chat (in-memory or still on disk).  The Artefacts view
    /// filters the sidebar on this so chats with nothing to read
    /// don't clutter the list.
    pub(crate) has_artefacts: bool,
    /// Origin of the chat.  HTTP-minted chats have ids of the form
    /// `c-NNNN` (see `mint_id`); Telegram chats are the numeric
    /// Telegram chat id as a string.  The UI badges Telegram rows so
    /// the operator can tell at a glance where a conversation came
    /// from when both transports share the same on-disk chat dir.
    pub(crate) source: &'static str,
}

#[derive(Serialize)]
pub(crate) struct MessageDto {
    pub(crate) role: String,
    pub(crate) blocks: Vec<BlockDto>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BlockDto {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// User-uploaded image or document reconstituted from chat history
    /// on reload.  The web UI's `FileBlock` component expects this
    /// shape and renders images inline (`<img>` when `inline_image`)
    /// and other files as download chips.  `url` is a data URL so the
    /// transcript is self-contained — chat history already externalises
    /// the bytes to `{chat_dir}/media/<hash>.b64` and restores them to
    /// inline base64 on load; we just repackage the base64 as a
    /// `data:<mime>;base64,…` URL instead of surfacing the raw bytes.
    File {
        name: String,
        mime: String,
        bytes: usize,
        url: String,
        inline_image: bool,
    },
    /// Reference to an artefact rendered in the Artefacts tab.  The body
    /// lives in `HttpState.artefacts` and is fetched via
    /// `/api/artefacts/<id>` — not inlined here to keep history payloads
    /// small when a chat has produced multiple long reports.
    Artefact {
        id: String,
        kind: crate::message::ArtefactKind,
        title: String,
        /// `/#/artefacts/<id>` — an SPA deep-link that opens the reader
        /// directly.  We intentionally do NOT hand out the raw
        /// `/api/artefacts/<id>` bytes URL here: cmd-click / copy-paste
        /// of the chip should land in a viewer, not on raw markdown.
        /// The reader still fetches the body through the API endpoint;
        /// the client constructs that URL itself from the id.
        url: String,
        bytes: usize,
        /// The originating tool call, when known.  The client uses
        /// this to hydrate an image-kind artefact into the matching
        /// `image_generate` tool panel on chat reload.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// Optional structured metadata — for image artefacts this
        /// carries `file_url` so the reader and the tool panel can
        /// render the image without a second round-trip.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },
}

#[derive(Deserialize)]
pub(crate) struct CreateChatBody {
    pub(crate) title: Option<String>,
    /// Chat id whose on-disk transcript should be rotated (archived)
    /// before the new conversation is minted.  The web sidebar's
    /// "+ New Conversation" button passes the currently-active chat id
    /// so starting a fresh chat also preserves the prior one as a
    /// dated archive — same shape Telegram gets on `/clear`.
    #[serde(default)]
    pub(crate) rotate_previous: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct TurnBody {
    pub(crate) prompt: String,
    /// Optional file attachments — base64-encoded bytes plus MIME type
    /// and original filename.  Resolved to multimodal `ContentBlock`s
    /// by `media::resolve_attachment` in `Agent::run_with_attachments`,
    /// the same path Telegram uses for photos / voice notes / docs.
    #[serde(default)]
    pub(crate) attachments: Vec<AttachmentDto>,
}

#[derive(Deserialize)]
pub(crate) struct AttachmentDto {
    /// MIME type — drives the resolver (image/* → resize, audio/* →
    /// transcribe, application/pdf → extract, text-like → wrap).
    pub(crate) mime_type: String,
    /// Original filename, if available.  Surfaces in the prompt so the
    /// model knows which file it is looking at.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Base64-encoded bytes, NO data-URL prefix (`data:image/png;base64,`
    /// must be stripped client-side).
    pub(crate) data_base64: String,
}

/// Maximum total request body for `POST /turn`.  Big enough for a
/// photo or a small PDF; refuses anything that would require streaming
/// uploads (which the controller doesn't do — body is buffered into
/// memory before deserialize).
pub(crate) const MAX_TURN_BODY: usize = 25 * 1024 * 1024;

/// Cap for control-plane JSON bodies (chat create, feedback, model
/// switch).  These payloads are tiny in practice — a title string, a
/// turn index, a provider/model name — so 16 KiB is plenty of slack
/// without giving an attacker a free megabyte to keep a worker busy.
pub(crate) const MAX_SMALL_BODY: usize = 16 * 1024;

/// Cap for `POST /api/mind/file` — workspace files are bigger than
/// the control-plane payloads (a SOUL.md or notes file can run to a
/// few MB) but we still refuse anything that would require streaming.
pub(crate) const MAX_MIND_BODY: usize = 4 * 1024 * 1024;

/// Events streamed over SSE for one conversation.
///
/// Keep these stable — the prototype's bridge.js parses them.  `view` on
/// `tool_result` is the typed payload that the right-rail panel renders
/// natively (terminal / diff / sbom / taint / read).  Tools without a
/// view leave it `None` and the panel falls back to plain text.
///
/// `parent_tool_id` on `tool_start` / `tool_result` / `file` / `artefact`
/// is set when the event was emitted by a subagent's `CaptureOutput` —
/// it carries the parent agent's `tool_use_id` (the subagent tool box
/// the inner call belongs to) so the frontend can render nested tool
/// chips inside the subagent panel instead of as new top-level chips.
/// Top-level (parent-agent) emissions leave it `None`.  Nested
/// `tool_result` also carries the inner call's own `tool_use_id` so the
/// frontend can update the right child without relying on the
/// most-recently-started liveToolRef heuristic — important when a
/// subagent dispatches tool calls in parallel.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    Text {
        delta: String,
    },
    /// A fragment of the model's extended-thinking / reasoning stream.
    /// Rendered in a dedicated right-rail panel — see `ThinkingPanel`
    /// in the web UI.  Arrives before any `text` event on turns where
    /// the model reasons before answering.
    Thinking {
        delta: String,
    },
    ToolStart {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_id: Option<String>,
    },
    ToolResult {
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        view: Option<ToolView>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_id: Option<String>,
        /// The id of the tool call that produced this result.  Set on
        /// nested (subagent-internal) emissions so the frontend can
        /// pick the correct child by id; left `None` for parent-level
        /// emissions where the legacy "most recent ToolStart" path
        /// suffices.
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
    Checkpoint {
        text: String,
    },
    /// An agent-produced file the UI can preview/download — points at
    /// `/api/files/<id>` served from the controller's in-memory file
    /// store.  `inline_image` is `true` for images so the UI can render
    /// `<img>` directly instead of a download link.
    File {
        name: String,
        mime_type: String,
        url: String,
        inline_image: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_id: Option<String>,
    },
    /// An agent-produced artefact (e.g. a security-review report) ready
    /// for full-page markdown rendering.  The body is served at
    /// `/api/artefacts/<id>` and the metadata list at
    /// `/api/conversations/<chat>/artefacts`.
    Artefact {
        id: String,
        kind: crate::message::ArtefactKind,
        title: String,
        url: String,
        bytes: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_id: Option<String>,
    },
    LlmError {
        message: String,
    },
    Done,
}

#[derive(Serialize)]
pub(crate) struct ProviderDto {
    pub(crate) id: String,
    pub(crate) name: String,
    /// All models configured for this provider in dyson.json, plus any
    /// added via POST /api/providers/:id/models during this session.
    pub(crate) models: Vec<String>,
    /// Currently-active model name for this provider (the agent-level
    /// `model` setting when this provider is the default; otherwise the
    /// first configured model).
    pub(crate) active_model: String,
    /// `true` if this is the default provider configured in dyson.json.
    pub(crate) active: bool,
}

/// Public auth metadata the SPA needs to bootstrap.  Lives next to
/// `auth: Arc<dyn Auth>` because the actual `Auth` trait hides
/// everything except `validate_request` / `apply_to_request` — the
/// discovery URL etc. don't belong in there.
///
/// Surfaced from `test_helpers::AuthMode` so integration tests can
/// pin a specific mode and assert on the SPA-facing summary
/// (`/api/auth/config`, the WWW-Authenticate header) without needing
/// a real OIDC IdP behind the rig.  `#[doc(hidden)]` keeps it out of
/// the published surface — the type is only `pub` for that test
/// hook.
#[doc(hidden)]
#[derive(Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthMode {
    None,
    Bearer,
    Oidc {
        issuer: String,
        authorization_endpoint: String,
        /// Where the SPA POSTs the `code` it gets back from the IdP.
        /// `None` if the provider's `.well-known` document didn't list
        /// one — pure-implicit-flow IdPs are rare but possible.
        token_endpoint: Option<String>,
        /// The OAuth `client_id` the SPA should send on `/authorize`.
        /// Same value the controller validates as the JWT `aud` claim.
        client_id: String,
        /// Scopes the operator told us are required.  The SPA appends
        /// `openid` automatically — that one is mandatory for OIDC.
        required_scopes: Vec<String>,
    },
}

/// Wire shape for `GET /api/conversations/<chat>/artefacts`.  One entry
/// per artefact emitted for this chat, ordered newest first.  The
/// reader fetches the body separately from `/api/artefacts/<id>` so the
/// list is cheap to render even when reports are multi-KB.
#[derive(Serialize)]
pub(crate) struct ArtefactDto {
    pub(crate) id: String,
    pub(crate) kind: crate::message::ArtefactKind,
    pub(crate) title: String,
    pub(crate) bytes: usize,
    pub(crate) created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct FeedbackBody {
    pub(crate) turn_index: usize,
    /// Emoji to map to a rating (matches the Telegram controller's set).
    /// When omitted or empty, removes any existing feedback for this turn.
    #[serde(default)]
    pub(crate) emoji: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct MindWriteBody {
    pub(crate) path: String,
    pub(crate) content: String,
}

#[derive(Deserialize)]
pub(crate) struct ModelSwitchBody {
    /// Provider name from `dyson.json` providers table.
    pub(crate) provider: String,
    /// Optional model — defaults to the provider's first configured model.
    pub(crate) model: Option<String>,
    /// Optional chat to swap on.  When omitted, swaps every loaded chat.
    pub(crate) chat_id: Option<String>,
}
