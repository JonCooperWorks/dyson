// ===========================================================================
// Audio transcription — pluggable speech-to-text via the Transcriber trait.
//
// Architecture:
//   media::resolve()
//     └── Arc<dyn Transcriber> (trait)
//           ├── WhisperCliTranscriber  (local `whisper` CLI)
//           └── (future: cloud APIs, whisper.cpp, etc.)
//
// The default implementation shells out to the `whisper` CLI (from
// openai-whisper).  This keeps transcription local and private — no audio
// data is sent to any external API.
//
// If Whisper is not installed, returns a descriptive error message as
// text rather than failing the entire request.  This allows the system
// to degrade gracefully.
// ===========================================================================

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Transcriber trait — pluggable speech-to-text backend.
// ---------------------------------------------------------------------------

/// A pluggable speech-to-text backend.
///
/// Implementations handle transcription for a specific engine or API.
/// The `media::resolve()` function delegates to this trait and handles
/// input/output formatting.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe raw audio bytes to text.
    ///
    /// `data` is the raw audio file content and `mime_type` is its MIME
    /// type (e.g. `"audio/ogg"`, `"audio/mpeg"`).
    async fn transcribe(&self, data: &[u8], mime_type: &str) -> crate::Result<String>;
}

// ---------------------------------------------------------------------------
// WhisperCliTranscriber — shells out to local `whisper` binary.
// ---------------------------------------------------------------------------

/// Transcribes audio by shelling out to the local `whisper` CLI.
///
/// Uses the openai-whisper Python package.  Install with:
/// ```sh
/// pip install openai-whisper
/// ```
///
/// The `model` field controls which Whisper model to use (e.g. "tiny",
/// "base", "small", "medium", "large").  Defaults to "base".
pub struct WhisperCliTranscriber {
    model: String,
}

impl WhisperCliTranscriber {
    pub fn new(model: Option<String>) -> Self {
        Self {
            model: model.unwrap_or_else(|| "base".into()),
        }
    }
}

#[async_trait]
impl Transcriber for WhisperCliTranscriber {
    async fn transcribe(&self, data: &[u8], mime_type: &str) -> crate::Result<String> {
        // Determine file extension from MIME type.
        let ext = match mime_type {
            "audio/ogg" => "ogg",
            "audio/mpeg" => "mp3",
            "audio/mp4" => "m4a",
            "audio/wav" | "audio/x-wav" => "wav",
            "audio/webm" => "webm",
            "audio/flac" => "flac",
            _ => "ogg", // OGG is a common default for voice messages.
        };

        // Check if whisper is available.
        let whisper_path = find_whisper().await.ok_or_else(|| {
            crate::DysonError::Config(
                "Whisper is not installed. Voice transcription requires the openai-whisper \
                 package. Install it with: pip install openai-whisper"
                    .to_string(),
            )
        })?;

        // Write audio to a temp file.
        let tmp_dir = std::env::temp_dir();
        let file_id = format!("{:016x}", rand::random::<u64>());
        let audio_path = tmp_dir.join(format!("dyson_audio_{file_id}.{ext}"));

        tokio::fs::write(&audio_path, data)
            .await
            .map_err(crate::DysonError::Io)?;

        // Run whisper.
        let output = Command::new(&whisper_path)
            .arg(&audio_path)
            .arg("--model")
            .arg(&self.model)
            .arg("--output_format")
            .arg("txt")
            .arg("--output_dir")
            .arg(&tmp_dir)
            .output()
            .await
            .map_err(crate::DysonError::Io)?;

        // Read the output transcript.
        let txt_path = tmp_dir.join(format!("dyson_audio_{file_id}.txt"));

        let transcript = if txt_path.exists() {
            tokio::fs::read_to_string(&txt_path)
                .await
                .map_err(crate::DysonError::Io)?
                .trim()
                .to_string()
        } else {
            // If whisper failed, capture stderr for diagnostics.
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                exit_code = ?output.status.code(),
                stderr = %stderr,
                "whisper transcription failed"
            );
            return Err(crate::DysonError::Config(format!(
                "Whisper transcription failed: {stderr}"
            )));
        };

        // Clean up temp files.
        let _ = tokio::fs::remove_file(&audio_path).await;
        let _ = tokio::fs::remove_file(&txt_path).await;

        tracing::info!(
            transcript_len = transcript.len(),
            model = self.model.as_str(),
            "audio transcription complete"
        );

        Ok(transcript)
    }
}

// ---------------------------------------------------------------------------
// Factory — build a transcriber from config.
// ---------------------------------------------------------------------------

/// Create a transcriber from configuration.
///
/// If no config is provided, defaults to `WhisperCliTranscriber` with the
/// "base" model — matching the previous hardcoded behavior.
pub fn create_transcriber(
    config: Option<&crate::config::TranscriberConfig>,
) -> Arc<dyn Transcriber> {
    match config {
        Some(cfg) => match cfg.provider.as_str() {
            "whisper" | "whisper-cli" => {
                Arc::new(WhisperCliTranscriber::new(cfg.model.clone()))
            }
            other => {
                tracing::warn!(
                    provider = other,
                    "unknown transcriber provider, falling back to whisper-cli"
                );
                Arc::new(WhisperCliTranscriber::new(cfg.model.clone()))
            }
        },
        None => Arc::new(WhisperCliTranscriber::new(None)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the whisper binary on the system.
async fn find_whisper() -> Option<PathBuf> {
    let output = Command::new("which").arg("whisper").output().await.ok()?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    None
}
