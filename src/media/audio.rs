// ===========================================================================
// Audio transcription — local Whisper-based speech-to-text.
//
// Shells out to the `whisper` CLI (from openai-whisper) to transcribe
// audio files.  This keeps transcription local and private — no audio
// data is sent to any external API.
//
// If Whisper is not installed, returns a descriptive error message as
// text rather than failing the entire request.  This allows the system
// to degrade gracefully.
// ===========================================================================

use std::path::PathBuf;
use tokio::process::Command;

/// Transcribe audio bytes to text using local Whisper.
///
/// Writes the audio to a temporary file, runs `whisper` on it, reads the
/// output transcript, and cleans up.  Returns the transcription text.
///
/// If Whisper is not installed, returns an error message explaining how
/// to install it rather than propagating a hard failure.
pub async fn transcribe(data: &[u8], mime_type: &str) -> crate::Result<String> {
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
    let whisper_path = find_whisper().await;
    if whisper_path.is_none() {
        return Err(crate::DysonError::Config(
            "Whisper is not installed. Voice transcription requires the openai-whisper \
             package. Install it with: pip install openai-whisper"
                .to_string(),
        ));
    }
    let whisper_path = whisper_path.unwrap();

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
        .arg("base")
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
        "audio transcription complete"
    );

    Ok(transcript)
}

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
