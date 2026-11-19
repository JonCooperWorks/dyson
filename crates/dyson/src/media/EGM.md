# Media Resolver (`media`)

## Overview
Ingestion pipeline for non-text input. Controllers download media and pass raw bytes plus MIME type to `resolve_attachment`, which dispatches by MIME prefix: images are resized and base64-encoded, audio is transcribed, PDFs are text-extracted, and Office documents are rendered to Markdown. Images, PDFs and Office documents are processed locally; audio is transcribed by the voice provider.

## Interface
- `Attachment` - controller-agnostic raw media [mod.rs:Attachment]
- `resolve_attachment(attachment, transcriber) -> Result<Vec<ContentBlock>>` [mod.rs:resolve_attachment]
- Re-exports MIME/extension helpers (`is_office_extension`, `is_office_mime`, `is_text_extension`, `is_text_like_mime`, ...) from `dyson_telegram::media` [mod.rs]
- `image::process_image(data)` [image.rs:process_image]
- `audio::Transcriber` trait, `audio::VoiceClient` (speech via `speak`, transcription via `Transcriber`) resolved from `Settings::voice`, `create_transcriber(settings)` [audio.rs:Transcriber, audio.rs:VoiceClient, audio.rs:create_transcriber]
- `pdf::process_pdf(data)` [pdf.rs:process_pdf]
- `office::process_office(data, file_name)` (via `undoc`) [office.rs:process_office]

## Invariants
- Images larger than `MAX_DIMENSION = 1568` px on either side are resized (Lanczos3); results over `MAX_BASE64_BYTES = 4 MiB` are rejected [image.rs:MAX_DIMENSION, image.rs:MAX_BASE64_BYTES]
- PDFs over `MAX_PDF_BYTES = 32 MiB` are rejected [pdf.rs:MAX_PDF_BYTES]
- Audio without a configured transcriber is an error [mod.rs:resolve_attachment]
- Voice uses only an OpenAI or OpenRouter provider with a key: `voice.provider`, else the image generation provider, else the active provider [audio.rs:VoiceClient::from_settings]
- Transcription uploads at most 25 MiB, asks for `json`; speech text is at most 4096 characters, returned as MP3 [audio.rs:MAX_TRANSCRIPTION_BYTES, audio.rs:MAX_SPEECH_CHARS]
- Office documents that render to empty Markdown are rejected [office.rs:process_office]

## Side Effects
- Image processing runs on `spawn_blocking` [mod.rs:resolve_attachment]
- Outbound HTTP to the voice provider's `/v1/audio/speech` and `/v1/audio/transcriptions` [audio.rs:VoiceClient]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
