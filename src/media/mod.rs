// ===========================================================================
// Media resolver — converts raw media bytes into ContentBlocks.
//
// This module is the ingestion pipeline for non-text input.  Controllers
// download media files and pass them here.  The resolver
// converts them to ContentBlocks that LLM providers can consume:
//
//   - Images  →  resize + base64  →  ContentBlock::Image
//   - Audio   →  Transcriber trait  →  ContentBlock::Text
//   - PDFs    →  text extract + base64  →  ContentBlock::Document
//
// The pipeline is intentionally local-first: images are processed in-process
// via the `image` crate, PDFs via `pdf-extract`, and audio transcription
// defaults to a local Whisper installation.  The `Transcriber` trait allows
// plugging in alternative backends (cloud APIs, whisper.cpp, etc.).
//
// Two public APIs:
//
//   resolve_attachment(Attachment, Option<transcriber>)
//       High-level: takes raw bytes + MIME type, dispatches by MIME prefix.
//       This is the primary API for the agent layer.
//
//   resolve(MediaInput, transcriber)
//       Lower-level: takes a typed MediaInput enum.  Used internally and
//       by controllers that want explicit type routing.
// ===========================================================================

pub mod audio;
pub mod image;
pub mod pdf;

use std::sync::Arc;

use crate::message::ContentBlock;

// ---------------------------------------------------------------------------
// Attachment — controller-agnostic raw media.
// ---------------------------------------------------------------------------

/// Raw media attachment from a controller.
///
/// Controllers download media from their protocol (Telegram API, HTTP upload,
/// filesystem) and pass raw bytes here.  The agent resolves attachments into
/// ContentBlocks before the LLM call.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Raw file bytes.
    pub data: Vec<u8>,
    /// MIME type (e.g. `"image/jpeg"`, `"audio/ogg"`, `"application/pdf"`).
    pub mime_type: String,
}

/// Resolve a raw attachment into ContentBlocks for the LLM.
///
/// Dispatches by MIME type:
/// - `image/*`  → resize + base64 → `ContentBlock::Image`
/// - `audio/*`  → transcribe → `ContentBlock::Text` (requires transcriber)
/// - `application/pdf` → extract + base64 → `ContentBlock::Document`
///
/// Returns an error if audio is provided but no transcriber is available,
/// or if the MIME type is unrecognized.
pub async fn resolve_attachment(
    attachment: Attachment,
    transcriber: Option<&Arc<dyn audio::Transcriber>>,
) -> crate::Result<Vec<ContentBlock>> {
    let mime = attachment.mime_type.as_str();
    if mime.starts_with("image/") {
        let block = image::process_image(&attachment.data)?;
        Ok(vec![block])
    } else if mime.starts_with("audio/") {
        let t = transcriber.ok_or_else(|| {
            crate::DysonError::Config(
                "audio attachment received but no transcriber configured".into(),
            )
        })?;
        let text = t.transcribe(&attachment.data, mime).await?;
        Ok(vec![ContentBlock::Text { text }])
    } else if mime == "application/pdf" {
        let block = pdf::process_pdf(&attachment.data)?;
        Ok(vec![block])
    } else {
        Err(crate::DysonError::Config(format!(
            "unsupported media type: {mime}"
        )))
    }
}

// ---------------------------------------------------------------------------
// Lower-level typed API (used internally and by resolve_attachment).
// ---------------------------------------------------------------------------

/// Raw media input from a controller.
pub enum MediaInput {
    /// A raw image (JPEG, PNG, WebP).
    Image { data: Vec<u8>, mime_type: String },
    /// A raw audio file (OGG/Opus voice messages, MP3, WAV, etc.).
    Audio { data: Vec<u8>, mime_type: String },
    /// A PDF document.
    Pdf { data: Vec<u8> },
}

/// Resolved media ready for the message pipeline.
pub enum ResolvedMedia {
    /// One or more image content blocks.
    Images(Vec<ContentBlock>),
    /// A text transcription of audio.
    Transcription(String),
    /// A processed PDF document (base64 + extracted text).
    Document(ContentBlock),
}

/// Resolve raw media into ContentBlocks.
///
/// Images are resized and base64-encoded.  Audio is transcribed via the
/// provided `Transcriber` implementation.
pub async fn resolve(
    input: MediaInput,
    transcriber: &Arc<dyn audio::Transcriber>,
) -> crate::Result<ResolvedMedia> {
    match input {
        MediaInput::Image { data, .. } => {
            let block = image::process_image(&data)?;
            Ok(ResolvedMedia::Images(vec![block]))
        }
        MediaInput::Audio { data, mime_type } => {
            let text = transcriber.transcribe(&data, &mime_type).await?;
            Ok(ResolvedMedia::Transcription(text))
        }
        MediaInput::Pdf { data } => {
            let block = pdf::process_pdf(&data)?;
            Ok(ResolvedMedia::Document(block))
        }
    }
}
