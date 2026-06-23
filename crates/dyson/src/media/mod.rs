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
// The public entry point is `resolve_attachment(Attachment, Option<transcriber>)`:
// it takes raw bytes + MIME type and dispatches by MIME prefix.  Controllers
// download media from their protocol and hand it here before the LLM call.
// ===========================================================================

pub mod audio;
pub mod image;
pub mod office;
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
    /// Original filename, if available.  Used to label text attachments in
    /// the prompt so the model knows which file it is looking at.
    pub file_name: Option<String>,
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
        let data = attachment.data;
        let block = tokio::task::spawn_blocking(move || image::process_image(&data))
            .await
            .map_err(|e| crate::DysonError::Config(format!("image task panicked: {e}")))??;
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
        let data = attachment.data;
        let block = tokio::task::spawn_blocking(move || pdf::process_pdf(&data))
            .await
            .map_err(|e| crate::DysonError::Config(format!("PDF task panicked: {e}")))??;
        Ok(vec![block])
    } else if is_office_mime(mime) {
        let file_name = attachment.file_name.clone();
        let data = attachment.data;
        let block = tokio::task::spawn_blocking(move || {
            office::process_office(&data, file_name.as_deref())
        })
        .await
        .map_err(|e| crate::DysonError::Config(format!("Office task panicked: {e}")))??;
        Ok(vec![block])
    } else if is_text_like_mime(mime) {
        let text = std::str::from_utf8(&attachment.data).map_err(|_| {
            crate::DysonError::Config(format!(
                "attachment {} is labelled as text but is not valid UTF-8",
                attachment.file_name.as_deref().unwrap_or("<unnamed>")
            ))
        })?;
        let label = attachment.file_name.as_deref().unwrap_or("attachment");
        let wrapped = format!("=== file: {label} ({mime}) ===\n{text}");
        Ok(vec![ContentBlock::Text { text: wrapped }])
    } else {
        Err(crate::DysonError::Config(format!(
            "unsupported media type: {mime}"
        )))
    }
}

/// True if a MIME type is one we treat as inline UTF-8 text.
///
/// Accepts anything under `text/*` and a curated list of text-shaped
/// `application/*` types.  Callers should normalize empty/unknown MIME
/// strings to a sensible default (e.g. `text/plain`) before calling.
pub fn is_text_like_mime(mime: &str) -> bool {
    if mime.starts_with("text/") {
        return true;
    }
    matches!(
        mime,
        "application/json"
            | "application/xml"
            | "application/javascript"
            | "application/x-yaml"
            | "application/yaml"
            | "application/toml"
            | "application/x-sh"
            | "application/x-shellscript"
    )
}

/// True if a MIME type corresponds to a Microsoft Office format we can extract
/// text from (docx, xlsx, pptx).
pub fn is_office_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/msword"
            | "application/vnd.ms-excel"
            | "application/vnd.ms-powerpoint"
    )
}

/// True if `ext` (lowercase, no leading dot) names a Microsoft Office
/// document we can extract text from.  Pair with [`is_office_mime`] when
/// classifying a file that may arrive with a missing or `application/
/// octet-stream` MIME type.
pub fn is_office_extension(ext: &str) -> bool {
    matches!(ext, "docx" | "xlsx" | "pptx" | "doc" | "xls" | "ppt")
}

/// True if `ext` (lowercase, no leading dot) names a file type we can
/// safely ingest as inline UTF-8 text.  Co-located with [`is_text_like_mime`]
/// so the MIME and extension whitelists evolve together.
pub fn is_text_extension(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown"
            | "txt"
            | "rst"
            | "log"
            | "csv"
            | "tsv"
            | "json"
            | "jsonl"
            | "ndjson"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "env"
            | "rs"
            | "go"
            | "py"
            | "pyi"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "rb"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "hh"
            | "cxx"
            | "hxx"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "m"
            | "mm"
            | "php"
            | "pl"
            | "lua"
            | "sql"
            | "html"
            | "htm"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "xml"
            | "svg"
            | "dockerfile"
            | "makefile"
            | "mk"
            | "lock"
            | "sum"
            | "mod"
            | "gitignore"
            | "gitattributes"
            | "editorconfig"
            | "r"
            | "scala"
            | "clj"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "elm"
            | "dart"
            | "vue"
            | "svelte"
            | "tf"
            | "hcl"
            | "proto"
            | "graphql"
            | "gql"
    )
}
