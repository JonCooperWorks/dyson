// ===========================================================================
// Media resolver — converts raw media bytes into ContentBlocks.
//
// This module is the ingestion pipeline for non-text input.  Controllers
// (e.g. Telegram) download media files and pass them here.  The resolver
// converts them to ContentBlocks that LLM providers can consume:
//
//   - Images  →  resize + base64  →  ContentBlock::Image
//   - Audio   →  Whisper transcription  →  ContentBlock::Text
//
// The pipeline is intentionally local-first: images are processed in-process
// via the `image` crate, and audio transcription shells out to a local
// Whisper installation.  No media leaves the machine unless a cloud LLM
// provider is invoked downstream.
// ===========================================================================

pub mod audio;
pub mod image;

use crate::message::ContentBlock;

/// Raw media input from a controller.
pub enum MediaInput {
    /// A raw image (JPEG, PNG, WebP, GIF).
    Image {
        data: Vec<u8>,
        mime_type: String,
    },
    /// A raw audio file (OGG/Opus from Telegram voice notes, etc.).
    Audio {
        data: Vec<u8>,
        mime_type: String,
    },
}

/// Resolved media ready for the message pipeline.
pub enum ResolvedMedia {
    /// One or more image content blocks.
    Images(Vec<ContentBlock>),
    /// A text transcription of audio.
    Transcription(String),
}

/// Resolve raw media into ContentBlocks.
///
/// Images are resized and base64-encoded.  Audio is transcribed via local
/// Whisper.  If an external tool (Whisper) is missing, a descriptive error
/// message is returned as text rather than failing the request.
pub async fn resolve(input: MediaInput) -> crate::Result<ResolvedMedia> {
    match input {
        MediaInput::Image { data, .. } => {
            let block = image::process_image(&data)?;
            Ok(ResolvedMedia::Images(vec![block]))
        }
        MediaInput::Audio { data, mime_type } => {
            let text = audio::transcribe(&data, &mime_type).await?;
            Ok(ResolvedMedia::Transcription(text))
        }
    }
}
