// ===========================================================================
// Media resolver — converts raw media bytes into ContentBlocks.
//
// This module is the ingestion pipeline for non-text input.  Controllers
// download media files and pass them here.  The resolver
// converts them to ContentBlocks that LLM providers can consume:
//
//   - Images  →  resize + base64  →  ContentBlock::Image
//   - Audio   →  Transcriber trait  →  ContentBlock::Text
//
// The pipeline is intentionally local-first: images are processed in-process
// via the `image` crate, and audio transcription defaults to a local
// Whisper installation.  The `Transcriber` trait allows plugging in
// alternative backends (cloud APIs, whisper.cpp, etc.).
// ===========================================================================

pub mod audio;
pub mod image;

use std::sync::Arc;

use crate::message::ContentBlock;

/// Raw media input from a controller.
pub enum MediaInput {
    /// A raw image (JPEG, PNG, WebP, GIF).
    Image { data: Vec<u8>, mime_type: String },
    /// A raw audio file (OGG/Opus voice messages, MP3, WAV, etc.).
    Audio { data: Vec<u8>, mime_type: String },
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
    }
}
