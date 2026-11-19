// ===========================================================================
// Voice — speech output and transcription through an OpenAI-compatible
// audio API (`/v1/audio/speech`, `/v1/audio/transcriptions`).
//
//   media::resolve_attachment()  ──► Arc<dyn Transcriber>  ─┐
//   tool::speak::SpeakTool       ──► VoiceClient::speak()  ─┴─► provider
//
// The provider is an OpenAI or OpenRouter entry from `providers` (see
// `config::VoiceConfig`). On a Swarm-managed Dyson that is the Swarm
// proxy: the agent's own proxy token authenticates, and Swarm routes the
// call (to the operator's OpenAI account for `openai/*` voice models),
// meters it and bills the owner. Nothing is transcribed locally.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{LlmProvider, Settings};
use crate::error::{DysonError, Result};

/// The most text one speech call reads aloud (OpenAI's limit).
pub const MAX_SPEECH_CHARS: usize = 4096;

/// The largest audio file sent for transcription (OpenAI's and Swarm's
/// limit).
pub const MAX_TRANSCRIPTION_BYTES: usize = 25 * 1024 * 1024;

const DEFAULT_SPEECH_MODEL: &str = "gpt-4o-mini-tts";
const DEFAULT_TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";
const DEFAULT_VOICE: &str = "alloy";

// ---------------------------------------------------------------------------
// Transcriber trait — what `media::resolve_attachment` asks of voice input.
// ---------------------------------------------------------------------------

/// Speech to text for audio attachments.
// `#[async_trait]` emits a bare `#[must_use]` on boxed-future trait methods.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe raw audio bytes (`mime_type` e.g. `"audio/ogg"`) to text.
    async fn transcribe(&self, data: &[u8], mime_type: &str) -> Result<String>;
}

// ---------------------------------------------------------------------------
// VoiceClient
// ---------------------------------------------------------------------------

/// One OpenAI-compatible audio API: speaks text and transcribes audio.
pub struct VoiceClient {
    client: reqwest::Client,
    api_key: crate::auth::Credential,
    /// Base URL the client appends `/v1/audio/...` to, as the chat client
    /// appends `/v1/chat/completions` (e.g. `https://openrouter.ai/api`,
    /// or a Swarm proxy's `https://<host>/llm/openrouter`).
    base_url: String,
    speech_model: String,
    transcription_model: String,
    voice: String,
}

impl std::fmt::Debug for VoiceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceClient")
            .field("base_url", &self.base_url)
            .field("speech_model", &self.speech_model)
            .field("transcription_model", &self.transcription_model)
            .field("voice", &self.voice)
            .finish_non_exhaustive()
    }
}

impl VoiceClient {
    /// The voice client `settings` configure, or `None` when no OpenAI or
    /// OpenRouter provider is available for it.
    pub fn from_settings(settings: &Settings) -> Option<Arc<Self>> {
        let voice = &settings.voice;
        let named = |name: &String| {
            settings.providers.get(name).map(|p| {
                (
                    p.provider_type.clone(),
                    p.api_key.clone(),
                    p.base_url.clone(),
                )
            })
        };
        let candidates = [
            voice.provider.as_ref().and_then(named),
            settings
                .agent
                .image_generation_provider
                .as_ref()
                .and_then(named),
            Some((
                settings.agent.provider.clone(),
                settings.agent.api_key.clone(),
                settings.agent.base_url.clone(),
            )),
        ];
        let (provider_type, api_key, base_url) = candidates
            .into_iter()
            .flatten()
            .find(|(kind, key, _)| speaks_audio(kind) && !key.expose().is_empty())?;
        let base_url = base_url.unwrap_or_else(|| default_base(&provider_type).to_owned());
        let model = |configured: &Option<String>, default: &str| {
            configured
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| qualified(&provider_type, default))
        };
        Some(Arc::new(Self {
            client: crate::http::client().clone(),
            api_key,
            base_url,
            speech_model: model(&voice.speech_model, DEFAULT_SPEECH_MODEL),
            transcription_model: model(&voice.transcription_model, DEFAULT_TRANSCRIPTION_MODEL),
            voice: voice
                .voice
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_VOICE.to_owned()),
        }))
    }

    fn url(&self, operation: &str) -> String {
        format!(
            "{}/v1/audio/{operation}",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Read `text` aloud. Returns MP3 bytes.
    pub async fn speak(
        &self,
        text: &str,
        voice: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut body = serde_json::json!({
            "model": self.speech_model,
            "input": text,
            "voice": voice.filter(|v| !v.trim().is_empty()).unwrap_or(&self.voice),
            "response_format": "mp3",
        });
        if let Some(instructions) = instructions.filter(|i| !i.trim().is_empty()) {
            body["instructions"] = serde_json::Value::String(instructions.to_owned());
        }
        let response = self
            .client
            .post(self.url("speech"))
            .bearer_auth(self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| DysonError::tool("speak", format!("request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DysonError::tool(
                "speak",
                format!("speech failed ({status}): {}", error_text(response).await),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| DysonError::tool("speak", format!("reading audio failed: {e}")))?;
        if bytes.is_empty() {
            return Err(DysonError::tool("speak", "the provider returned no audio"));
        }
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl Transcriber for VoiceClient {
    async fn transcribe(&self, data: &[u8], mime_type: &str) -> Result<String> {
        if data.len() > MAX_TRANSCRIPTION_BYTES {
            return Err(DysonError::Config(format!(
                "audio too large to transcribe ({} MB; 25 MB at most)",
                data.len() / (1024 * 1024)
            )));
        }
        let (extension, mime) = audio_format(mime_type);
        let file = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(format!("audio.{extension}"))
            .mime_str(mime)
            .map_err(|e| DysonError::Config(format!("audio type: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.transcription_model.clone())
            .text("response_format", "json")
            .part("file", file);
        let response = self
            .client
            .post(self.url("transcriptions"))
            .bearer_auth(self.api_key.expose())
            .multipart(form)
            .send()
            .await
            .map_err(DysonError::Http)?;
        let status = response.status();
        if !status.is_success() {
            return Err(DysonError::Config(format!(
                "transcription failed ({status}): {}",
                error_text(response).await
            )));
        }
        let body: serde_json::Value = response.json().await.map_err(DysonError::Http)?;
        let transcript = body["text"].as_str().unwrap_or_default().trim().to_owned();
        tracing::info!(
            transcript_len = transcript.len(),
            model = self.transcription_model.as_str(),
            "audio transcription complete"
        );
        Ok(transcript)
    }
}

/// The transcriber `settings` configure, if any.
pub fn create_transcriber(settings: &Settings) -> Option<Arc<dyn Transcriber>> {
    VoiceClient::from_settings(settings).map(|client| client as Arc<dyn Transcriber>)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Provider types that speak the OpenAI audio API.
fn speaks_audio(kind: &LlmProvider) -> bool {
    matches!(kind, LlmProvider::OpenAi | LlmProvider::OpenRouter)
}

fn default_base(kind: &LlmProvider) -> &'static str {
    match kind {
        LlmProvider::OpenRouter => "https://openrouter.ai/api",
        _ => "https://api.openai.com",
    }
}

/// A default model id in the provider's naming: OpenRouter (and the Swarm
/// proxy behind it) namespaces OpenAI's models as `openai/<id>`.
fn qualified(kind: &LlmProvider, model: &str) -> String {
    match kind {
        LlmProvider::OpenRouter => format!("openai/{model}"),
        _ => model.to_owned(),
    }
}

/// File extension and media type to upload audio of `mime_type` as.
fn audio_format(mime_type: &str) -> (&'static str, &'static str) {
    match mime_type.split(';').next().unwrap_or_default().trim() {
        "audio/mpeg" | "audio/mp3" => ("mp3", "audio/mpeg"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => ("m4a", "audio/mp4"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => ("wav", "audio/wav"),
        "audio/webm" => ("webm", "audio/webm"),
        "audio/flac" => ("flac", "audio/flac"),
        // Telegram voice notes are Ogg Opus; the most common default.
        _ => ("ogg", "audio/ogg"),
    }
}

/// An error response's message, short enough for a tool result.
async fn error_text(response: reqwest::Response) -> String {
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(body);
    message.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_models_follow_the_provider_naming() {
        assert_eq!(
            qualified(&LlmProvider::OpenRouter, DEFAULT_SPEECH_MODEL),
            "openai/gpt-4o-mini-tts"
        );
        assert_eq!(
            qualified(&LlmProvider::OpenAi, DEFAULT_TRANSCRIPTION_MODEL),
            "gpt-4o-transcribe"
        );
        assert!(speaks_audio(&LlmProvider::OpenRouter));
        assert!(!speaks_audio(&LlmProvider::Anthropic));
    }

    #[test]
    fn uploads_are_named_by_their_media_type() {
        assert_eq!(audio_format("audio/ogg"), ("ogg", "audio/ogg"));
        assert_eq!(audio_format("audio/mpeg"), ("mp3", "audio/mpeg"));
        assert_eq!(audio_format("audio/x-wav; codecs=1"), ("wav", "audio/wav"));
        assert_eq!(audio_format("application/weird"), ("ogg", "audio/ogg"));
    }
}
