# Media Resolver (`media`)

## Overview
Ingestion pipeline for non-text input. Controllers download media and pass raw bytes plus MIME type to `resolve_attachment`, which dispatches by MIME prefix: images are resized and base64-encoded, audio is transcribed, PDFs are text-extracted, and Office documents are rendered to Markdown. Processing is local-first.

## Interface
- `Attachment` - controller-agnostic raw media [mod.rs:Attachment]
- `resolve_attachment(attachment, transcriber) -> Result<Vec<ContentBlock>>` [mod.rs:resolve_attachment]
- Re-exports MIME/extension helpers (`is_office_extension`, `is_office_mime`, `is_text_extension`, `is_text_like_mime`, ...) from `dyson_telegram::media` [mod.rs]
- `image::process_image(data)` [image.rs:process_image]
- `audio::Transcriber` trait, `WhisperCliTranscriber`, `create_transcriber(...)` [audio.rs:Transcriber, audio.rs:WhisperCliTranscriber, audio.rs:create_transcriber]
- `pdf::process_pdf(data)` [pdf.rs:process_pdf]
- `office::process_office(data, file_name)` (via `undoc`) [office.rs:process_office]

## Invariants
- Images larger than `MAX_DIMENSION = 1568` px on either side are resized (Lanczos3); results over `MAX_BASE64_BYTES = 4 MiB` are rejected [image.rs:MAX_DIMENSION, image.rs:MAX_BASE64_BYTES]
- PDFs over `MAX_PDF_BYTES = 32 MiB` are rejected [pdf.rs:MAX_PDF_BYTES]
- Audio without a configured transcriber is an error [mod.rs:resolve_attachment]
- Office documents that render to empty Markdown are rejected [office.rs:process_office]

## Side Effects
- Image processing runs on `spawn_blocking` [mod.rs:resolve_attachment]
- Whisper transcription writes `dyson_audio_<id>.<ext>` to the temp dir, runs the `whisper` CLI, reads the `.txt` output, and deletes both files [audio.rs:WhisperCliTranscriber]
- Locates whisper with `which whisper` [audio.rs:create_transcriber]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
