// ===========================================================================
// SpeakTool — read text aloud and deliver the audio to the user.
//
// The speech comes from the voice provider (`media::audio::VoiceClient`).
// Like `image_generate`, the result is a temp file attached with
// `ToolOutput::with_file`, so each controller delivers it its own way: an
// inline player in the web chat, an audio message in Telegram.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::media::audio::{MAX_SPEECH_CHARS, VoiceClient};
use crate::tool::{Tool, ToolContext, ToolOutput};
use dyson_core::error::Result;

pub struct SpeakTool {
    voice: Arc<VoiceClient>,
}

impl SpeakTool {
    pub fn new(voice: Arc<VoiceClient>) -> Self {
        Self { voice }
    }
}

#[async_trait]
impl Tool for SpeakTool {
    fn name(&self) -> &str {
        "speak"
    }

    fn description(&self) -> &str {
        "Say something out loud: turns text into speech and sends the user the \
         audio. Use it when the user asks you to speak, talk, read something \
         aloud, or reply by voice. The text is read exactly as written, so write \
         it to be heard (no markdown, code or URLs)."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "What to say, at most 4096 characters."
                },
                "voice": {
                    "type": "string",
                    "description": "Voice name, e.g. alloy, ash, ballad, coral, echo, fable, nova, onyx, sage, shimmer. Optional."
                },
                "instructions": {
                    "type": "string",
                    "description": "How to say it: tone, pace, accent, emotion. Optional."
                }
            },
            "required": ["text"]
        })
    }

    /// Speech changes nothing in the workspace, so it is not a mutation: a
    /// turn that only speaks finishes as completed, not partial (the
    /// default exclusive plan counts as a write). Not retryable: a
    /// recovered call would speak, and bill, twice.
    fn execution_plan(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> crate::tool::ToolExecutionPlan {
        crate::tool::ToolExecutionPlan {
            idempotency: crate::tool::Idempotency::Unsafe,
            ..crate::tool::ToolExecutionPlan::read("voice:speech")
        }
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let text = input["text"].as_str().unwrap_or_default().trim();
        if text.is_empty() {
            return Ok(ToolOutput::error("text is required"));
        }
        let chars = text.chars().count();
        if chars > MAX_SPEECH_CHARS {
            return Ok(ToolOutput::error(format!(
                "text too long ({chars} characters, {MAX_SPEECH_CHARS} at most). \
                 Split it into several calls."
            )));
        }
        let audio = tokio::select! {
            audio = self.voice.speak(text, input["voice"].as_str(), input["instructions"].as_str()) => audio,
            _ = ctx.cancellation.cancelled() => return Ok(ToolOutput::error("speech cancelled")),
        };
        let audio = match audio {
            Ok(audio) => audio,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        let path =
            std::env::temp_dir().join(format!("dyson_speech_{:016x}.mp3", rand::random::<u64>()));
        if let Err(error) = tokio::fs::write(&path, &audio).await {
            return Ok(ToolOutput::error(format!(
                "failed to save the audio: {error}"
            )));
        }
        Ok(ToolOutput::success(format!(
            "Spoke {chars} characters; the audio was sent to the user."
        ))
        .with_file(&path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(base: &str) -> Arc<VoiceClient> {
        let mut settings = crate::config::Settings::default();
        settings.agent.provider = crate::config::LlmProvider::OpenRouter;
        settings.agent.api_key = crate::auth::Credential::new("pt_test".into());
        settings.agent.base_url = Some(base.to_owned());
        VoiceClient::from_settings(&settings).expect("an OpenRouter provider speaks")
    }

    #[tokio::test]
    async fn speech_is_delivered_as_an_mp3_file() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/speech"))
            .and(header("authorization", "Bearer pt_test"))
            .and(body_partial_json(json!({
                "model": "openai/gpt-4o-mini-tts",
                "input": "Hello there",
                "voice": "nova",
                "response_format": "mp3",
                "instructions": "cheerful",
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/mpeg")
                    .set_body_bytes(b"ID3fake".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let tool = SpeakTool::new(client(&server.uri()));
        let ctx = ToolContext::from_cwd().unwrap();
        let out = tool
            .run(
                &json!({"text": "Hello there", "voice": "nova", "instructions": "cheerful"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.files[0].extension().unwrap(), "mp3");
        assert_eq!(std::fs::read(&out.files[0]).unwrap(), b"ID3fake");
        let _ = std::fs::remove_file(&out.files[0]);
    }

    #[tokio::test]
    async fn a_refusal_is_reported_to_the_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/speech"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": {"message": "Swarm has no price for the speech model `x`.", "code": "model_not_priced"}
            })))
            .mount(&server)
            .await;
        let tool = SpeakTool::new(client(&server.uri()));
        let ctx = ToolContext::from_cwd().unwrap();
        let out = tool.run(&json!({"text": "hi"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("no price"), "{}", out.content);
        assert!(out.files.is_empty());
    }

    #[tokio::test]
    async fn empty_and_overlong_text_never_reach_the_provider() {
        let server = MockServer::start().await;
        let tool = SpeakTool::new(client(&server.uri()));
        let ctx = ToolContext::from_cwd().unwrap();
        assert!(
            tool.run(&json!({"text": "  "}), &ctx)
                .await
                .unwrap()
                .is_error
        );
        let long = "a".repeat(MAX_SPEECH_CHARS + 1);
        assert!(
            tool.run(&json!({"text": long}), &ctx)
                .await
                .unwrap()
                .is_error
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn audio_is_transcribed_through_the_same_provider() {
        use crate::media::audio::Transcriber;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .and(header("authorization", "Bearer pt_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "text": " hello world ",
                "usage": {"type": "duration", "seconds": 2}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let voice = client(&server.uri());
        let text = voice
            .transcribe(b"OggS-voice-note", "audio/ogg")
            .await
            .unwrap();
        assert_eq!(text, "hello world");
        let request = &server.received_requests().await.unwrap()[0];
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains("name=\"model\"\r\n\r\nopenai/gpt-4o-transcribe"),
            "{body}"
        );
        assert!(body.contains("filename=\"audio.ogg\""), "{body}");
        assert!(body.contains("OggS-voice-note"));
    }

    /// Swarm refuses a `pt_` bearer without the pod's identity token
    /// (`401 workload identity mismatch`, as TARS first got).
    #[test]
    fn a_swarm_bearer_carries_the_workload_identity() {
        // Isolate the mounted-token override from concurrent tests.
        const CHILD: &str = "DYSON_SPEAK_IDENTITY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = std::thread::current().name().unwrap().to_owned();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("token"), "pod-identity-jwt\n").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test])
                .env(CHILD, "1")
                .env("DYSON_WORKLOAD_IDENTITY_FILE", dir.path().join("token"))
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/v1/audio/speech"))
                    .and(header(
                        crate::auth::workload_identity::HEADER,
                        "pod-identity-jwt",
                    ))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ID3".to_vec()))
                    .expect(1)
                    .mount(&server)
                    .await;
                let audio = client(&server.uri()).speak("hi", None, None).await.unwrap();
                assert_eq!(audio, b"ID3");
            });
    }

    #[test]
    fn speaking_is_not_a_workspace_mutation() {
        let tool = SpeakTool::new(client("http://127.0.0.1:9"));
        let ctx = ToolContext::from_cwd().unwrap();
        let plan = tool.execution_plan(&json!({"text": "hi"}), &ctx);
        assert!(
            plan.resources
                .iter()
                .all(|r| r.access == crate::tool::ResourceAccess::Read)
        );
        assert_eq!(plan.idempotency, crate::tool::Idempotency::Unsafe);
    }

    #[test]
    fn no_voice_without_an_openai_compatible_provider() {
        let mut settings = crate::config::Settings::default();
        settings.agent.provider = crate::config::LlmProvider::Anthropic;
        settings.agent.api_key = crate::auth::Credential::new("sk-ant".into());
        assert!(VoiceClient::from_settings(&settings).is_none());
        settings.voice.provider = Some("missing".into());
        assert!(VoiceClient::from_settings(&settings).is_none());
    }
}
