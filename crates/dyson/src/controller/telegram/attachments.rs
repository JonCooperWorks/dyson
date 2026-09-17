//! Translate downloadable Telegram media to agent attachments.
use super::{api::BotApi, types};
use crate::media;
use dyson_telegram::media::{DocumentKind, DownloadLimits, classify_document, effective_mime};

// ---------------------------------------------------------------------------
// Media extraction helpers
// ---------------------------------------------------------------------------

/// Download media attachments from a Telegram message.
///
/// Only handles Telegram-specific concerns: detecting media types from
/// message fields and downloading via the Bot API.  Media resolution
/// (resizing, transcription, PDF extraction) is handled by the agent
/// via `run_with_attachments()`.
pub(super) async fn extract_attachments(
    bot: &BotApi,
    msg: &types::Message,
    limits: &DownloadLimits,
) -> (Vec<media::Attachment>, Vec<String>) {
    let mut attachments = Vec::new();
    let mut skip_reasons: Vec<String> = Vec::new();

    // Photos: pick the largest resolution (last in the array).
    if let Some(photos) = &msg.photo
        && let Some(photo) = photos.last()
    {
        tracing::info!(
            file_id = photo.file_id.as_str(),
            width = photo.width,
            height = photo.height,
            "downloading photo from Telegram"
        );
        match bot
            .download_file(&photo.file_id, limits.image_max_bytes)
            .await
        {
            Ok(data) => {
                attachments.push(media::Attachment {
                    data,
                    mime_type: "image/jpeg".into(),
                    file_name: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "failed to download photo"),
        }
    }

    // Voice notes.
    if let Some(voice) = &msg.voice {
        tracing::info!(
            file_id = voice.file_id.as_str(),
            "downloading voice note from Telegram"
        );
        let mime = voice
            .mime_type
            .as_deref()
            .unwrap_or("audio/ogg")
            .to_string();
        match bot
            .download_file(&voice.file_id, limits.audio_max_bytes)
            .await
        {
            Ok(data) => {
                attachments.push(media::Attachment {
                    data,
                    mime_type: mime,
                    file_name: None,
                });
            }
            Err(e) => tracing::warn!(error = %e, "failed to download voice note"),
        }
    }

    // Documents: images, PDFs, Office, and text-like files.  Binaries are
    // rejected here without being downloaded at all.
    if let Some(doc) = &msg.document {
        let mime = doc.mime_type.as_deref().unwrap_or("").to_string();
        let file_name = doc.file_name.clone();
        let display_name = file_name.as_deref().unwrap_or("file").to_string();

        let kind = classify_document(&mime, file_name.as_deref());
        if matches!(kind, DocumentKind::Binary) {
            tracing::info!(
                file_name = display_name.as_str(),
                mime_type = mime.as_str(),
                "skipping binary document (not downloaded)"
            );
            skip_reasons.push(format!(
                "Skipped `{display_name}` — I can only read text files, Office docs, PDFs, and images."
            ));
        } else {
            let limit = limits.for_document(kind);
            tracing::info!(
                file_id = doc.file_id.as_str(),
                file_name = display_name.as_str(),
                mime_type = mime.as_str(),
                kind = ?kind,
                "downloading document from Telegram"
            );
            match bot.download_file(&doc.file_id, limit).await {
                Ok(data) => {
                    // Text documents need UTF-8 validation; reject if invalid.
                    if matches!(kind, DocumentKind::Text) && std::str::from_utf8(&data).is_err() {
                        tracing::warn!(
                            file_name = display_name.as_str(),
                            "not valid UTF-8 — dropping"
                        );
                        skip_reasons.push(format!(
                            "Skipped `{display_name}` — looked like text but isn't valid UTF-8."
                        ));
                    } else {
                        let effective_mime = effective_mime(&mime, kind, file_name.as_deref());
                        attachments.push(media::Attachment {
                            data,
                            mime_type: effective_mime,
                            file_name,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to download document");
                    skip_reasons.push(format!("Couldn't download `{display_name}`: {e}"));
                }
            }
        }
    }

    (attachments, skip_reasons)
}
