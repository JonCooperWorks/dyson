// ===========================================================================
// ImageGenerateTool — generate images with pluggable provider backends.
//
// Architecture:
//   ImageGenerateTool (implements Tool)
//     └── Arc<dyn ImageGenerationProvider> (trait)
//           └── GeminiImageProvider (Nano Banana 2)
//
// The Tool handles input parsing, output formatting, temp file management,
// and cancellation.  The ImageGenerationProvider trait handles the HTTP call
// and response parsing.
//
// Generated images are saved to temp files and delivered to the user via
// ToolOutput::with_file() — the controller's Output::send_file() delivers
// them (e.g. printing the path in terminal, sending a document in Telegram).
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

use crate::config::{LlmProvider, ProviderConfig};
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

// ---------------------------------------------------------------------------
// GeneratedImage — a single image returned by a provider.
// ---------------------------------------------------------------------------

/// A single generated image.
pub struct GeneratedImage {
    /// Raw image bytes (PNG, JPEG, etc.).
    pub data: Vec<u8>,
    /// MIME type of the image data (e.g., `"image/png"`).
    pub mime_type: String,
}

// ---------------------------------------------------------------------------
// ImageGenerationProvider trait — pluggable image generation backend.
// ---------------------------------------------------------------------------

/// A pluggable image generation backend.
///
/// Implementations handle the HTTP call and response parsing for a specific
/// image generation API.  The `ImageGenerateTool` delegates to this trait
/// and handles input/output formatting.
#[async_trait]
pub trait ImageGenerationProvider: Send + Sync {
    /// Generate images from a text prompt.
    ///
    /// Returns one or more generated images.  `count` is the number of
    /// images to produce (providers may clamp this).  `resolution` is one
    /// of `"1K"`, `"2K"`, or `"4K"` (providers may ignore it).
    async fn generate(
        &self,
        prompt: &str,
        count: usize,
        resolution: &str,
    ) -> Result<Vec<GeneratedImage>>;
}

// ---------------------------------------------------------------------------
// GeminiImageProvider — Google Gemini (Nano Banana 2) API.
// ---------------------------------------------------------------------------

/// Google Gemini image generation provider.
///
/// Uses the Gemini `generateContent` API with `responseModalities: ["TEXT", "IMAGE"]`
/// and `imageConfig` to generate images.  Default model: `gemini-3-pro-image-preview`.
///
/// API: `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`
/// Auth: `x-goog-api-key` header.
pub struct GeminiImageProvider {
    client: reqwest::Client,
    api_key: crate::auth::Credential,
    model: String,
}

impl GeminiImageProvider {
    pub fn new(api_key: crate::auth::Credential, model: String) -> Self {
        Self {
            client: crate::http::client().clone(),
            api_key,
            model,
        }
    }
}

#[async_trait]
impl ImageGenerationProvider for GeminiImageProvider {
    async fn generate(
        &self,
        prompt: &str,
        count: usize,
        resolution: &str,
    ) -> Result<Vec<GeneratedImage>> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );

        let body = json!({
            "contents": [{
                "parts": [{
                    "text": prompt
                }]
            }],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"],
                "imageConfig": {
                    "imageSize": resolution
                }
            }
        });

        let mut images = Vec::new();
        let count = count.clamp(1, 4);

        // Gemini doesn't support a batch count parameter, so make
        // separate requests for each image.
        for _ in 0..count {
            let resp = self
                .client
                .post(&url)
                .header("x-goog-api-key", self.api_key.expose())
                .json(&body)
                .send()
                .await
                .map_err(|e| DysonError::tool("image_generate", format!("request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(DysonError::tool(
                    "image_generate",
                    format!("Gemini API returned {status}: {body}"),
                ));
            }

            let resp_body: serde_json::Value = resp.json().await.map_err(|e| {
                DysonError::tool("image_generate", format!("invalid JSON response: {e}"))
            })?;

            // Extract inline image data from response parts.
            // Response shape: candidates[].content.parts[].inlineData.{mimeType, data}
            if let Some(candidates) = resp_body["candidates"].as_array() {
                for candidate in candidates {
                    if let Some(parts) = candidate["content"]["parts"].as_array() {
                        for part in parts {
                            if let Some(inline_data) = part.get("inlineData") {
                                let mime = inline_data["mimeType"]
                                    .as_str()
                                    .unwrap_or("image/png")
                                    .to_string();
                                if !mime.starts_with("image/") {
                                    continue;
                                }
                                let b64 = inline_data["data"].as_str().unwrap_or("");
                                if b64.is_empty() {
                                    continue;
                                }
                                let data = base64::engine::general_purpose::STANDARD
                                    .decode(b64)
                                    .map_err(|e| {
                                        DysonError::tool(
                                            "image_generate",
                                            format!("invalid base64 in response: {e}"),
                                        )
                                    })?;
                                images.push(GeneratedImage {
                                    data,
                                    mime_type: mime,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Gemini may return multiple image parts per response (e.g. duplicate
        // candidates or extra inlineData parts).  Cap to the requested count
        // so the user doesn't receive duplicate images.
        images.truncate(count);

        if images.is_empty() {
            return Err(DysonError::tool(
                "image_generate",
                "provider returned no images in the response",
            ));
        }

        Ok(images)
    }
}

// ---------------------------------------------------------------------------
// OpenRouterImageProvider — OpenRouter's `chat/completions` with
// `modalities: ["image", "text"]`.  OpenRouter fronts every image-capable
// model behind one OpenAI-compatible URL, so the same transport that
// drives chat in `dyson swarm` mode can drive image generation through
// the swarm's `/llm/openrouter` proxy without a separate API key.
// ---------------------------------------------------------------------------

const OPENROUTER_DEFAULT_BASE: &str = "https://openrouter.ai/api";

/// OpenRouter image generation provider.
///
/// Posts to `<base_url>/v1/chat/completions` with `modalities`
/// requesting an image.  Response shape (per OpenRouter docs, mirrors
/// the OpenAI multimodal-output convention): `choices[].message.images[]
/// .image_url.url` is a `data:image/...;base64,...` URL.
pub struct OpenRouterImageProvider {
    client: reqwest::Client,
    api_key: crate::auth::Credential,
    model: String,
    base_url: String,
}

impl OpenRouterImageProvider {
    pub fn new(api_key: crate::auth::Credential, model: String, base_url: Option<String>) -> Self {
        Self {
            client: crate::http::client().clone(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| OPENROUTER_DEFAULT_BASE.into()),
        }
    }
}

#[async_trait]
impl ImageGenerationProvider for OpenRouterImageProvider {
    async fn generate(
        &self,
        prompt: &str,
        count: usize,
        _resolution: &str,
    ) -> Result<Vec<GeneratedImage>> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let body = json!({
            "model": self.model,
            "modalities": ["image", "text"],
            "messages": [{ "role": "user", "content": prompt }],
        });

        // OpenRouter's chat completions API returns a single assistant
        // message per request; `n` for image-output models is not
        // universally honoured, so emit `count` independent requests
        // and merge the results — matches the per-image fan-out the
        // Gemini provider does for the same reason.
        let mut images = Vec::new();
        for _ in 0..count.clamp(1, 4) {
            let resp = self
                .client
                .post(&url)
                .bearer_auth(self.api_key.expose())
                .json(&body)
                .send()
                .await
                .map_err(|e| DysonError::tool("image_generate", format!("request failed: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(DysonError::tool(
                    "image_generate",
                    format!("OpenRouter API returned {status}: {body}"),
                ));
            }

            let resp_body: serde_json::Value = resp.json().await.map_err(|e| {
                DysonError::tool("image_generate", format!("invalid JSON response: {e}"))
            })?;

            extend_with_openrouter_images(&mut images, &resp_body)?;
        }

        if images.is_empty() {
            return Err(DysonError::tool(
                "image_generate",
                "provider returned no images in the response",
            ));
        }
        Ok(images)
    }
}

/// Pull images out of an OpenRouter chat completion.  Walks
/// `choices[].message.images[].image_url.url` and decodes any
/// `data:image/...;base64,...` URLs we find.  Non-data URLs are not
/// fetched here — every image-output model on OpenRouter returns
/// inline base64 today, and a follow-up GET would punch through the
/// swarm proxy auth boundary.
fn extend_with_openrouter_images(
    out: &mut Vec<GeneratedImage>,
    resp_body: &serde_json::Value,
) -> Result<()> {
    let Some(choices) = resp_body["choices"].as_array() else {
        return Ok(());
    };
    for choice in choices {
        let Some(arr) = choice["message"]["images"].as_array() else {
            continue;
        };
        for img in arr {
            let url = img["image_url"]["url"].as_str().unwrap_or("");
            if !url.starts_with("data:") {
                continue;
            }
            let comma = match url.find(',') {
                Some(i) => i,
                None => continue,
            };
            let header = &url[5..comma]; // strip "data:"
            let mime = header.split(';').next().unwrap_or("image/png").to_string();
            if !mime.starts_with("image/") {
                continue;
            }
            let b64 = &url[comma + 1..];
            let data = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| {
                    DysonError::tool("image_generate", format!("invalid base64 in response: {e}"))
                })?;
            out.push(GeneratedImage {
                data,
                mime_type: mime,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Factory — create an ImageGenerationProvider from a ProviderConfig.
// ---------------------------------------------------------------------------

/// Create an `ImageGenerationProvider` from a provider configuration.
///
/// Dispatches on `provider_type` to select the right backend.
/// Currently supported: `Gemini`, `OpenRouter`.
///
/// When `model_override` is set, it takes precedence over the provider's
/// default model.  This lets users configure a chat model as the provider
/// default while using a different model for image generation.
pub fn create_provider(
    config: &ProviderConfig,
    model_override: Option<&str>,
) -> Result<Arc<dyn ImageGenerationProvider>> {
    let model = model_override
        .unwrap_or_else(|| config.default_model())
        .to_string();
    match config.provider_type {
        LlmProvider::Gemini => Ok(Arc::new(GeminiImageProvider::new(
            config.api_key.clone(),
            model,
        ))),
        LlmProvider::OpenRouter => Ok(Arc::new(OpenRouterImageProvider::new(
            config.api_key.clone(),
            model,
            config.base_url.clone(),
        ))),
        ref other => Err(DysonError::Config(format!(
            "provider type {other:?} does not support image generation \
             (supported: gemini, openrouter)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// ImageGenerateTool — the Tool implementation.
// ---------------------------------------------------------------------------

/// Built-in tool that generates images via a pluggable provider.
pub struct ImageGenerateTool {
    provider: Arc<dyn ImageGenerationProvider>,
}

impl ImageGenerateTool {
    pub fn new(provider: Arc<dyn ImageGenerationProvider>) -> Self {
        Self { provider }
    }
}

/// Maximum prompt length in characters.
const MAX_PROMPT_LEN: usize = 10_000;

#[async_trait]
impl Tool for ImageGenerateTool {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn description(&self) -> &str {
        "Generate images from a text description. The generated images are \
         delivered to the user as files. Use this when the user asks you to \
         create, draw, generate, or design an image, illustration, diagram, \
         or visual content."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "A detailed description of the image to generate. \
                                    Be specific about style, content, composition, and mood."
                },
                "count": {
                    "type": "integer",
                    "description": "Number of images to generate (1-4, default 1)",
                    "minimum": 1,
                    "maximum": 4,
                    "default": 1
                },
                "resolution": {
                    "type": "string",
                    "enum": ["1K", "2K", "4K"],
                    "description": "Output image resolution (default 4K)",
                    "default": "4K"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let prompt = input["prompt"].as_str().unwrap_or("").to_string();

        if prompt.is_empty() {
            return Ok(ToolOutput::error("prompt is required"));
        }

        if prompt.len() > MAX_PROMPT_LEN {
            return Ok(ToolOutput::error(format!(
                "prompt too long ({} chars, max {MAX_PROMPT_LEN}). Shorten the prompt.",
                prompt.len(),
            )));
        }

        tracing::info!(prompt = prompt.as_str(), "image generation request");

        let count = input["count"].as_u64().unwrap_or(1).clamp(1, 4) as usize;
        let resolution = match input["resolution"].as_str() {
            Some("1K") => "1K",
            Some("2K") => "2K",
            _ => "4K",
        };

        // Race the generation against cancellation (Ctrl-C).
        let images = tokio::select! {
            res = self.provider.generate(&prompt, count, resolution) => res?,
            _ = ctx.cancellation.cancelled() => {
                return Ok(ToolOutput::error("image generation cancelled"));
            }
        };

        // Cap to the requested count — a provider may return extra images
        // (e.g. Gemini returning multiple inlineData parts per response).
        let images: Vec<_> = images.into_iter().take(count).collect();

        // Save generated images to temp files and attach them to the output.
        let random_suffix: u64 = rand::random();
        let mut output = ToolOutput::success(format!(
            "Generated {} image(s) for: \"{}\"",
            images.len(),
            super::truncate(&prompt, 100),
        ));

        for (i, image) in images.iter().enumerate() {
            let extension = match image.mime_type.as_str() {
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                _ => "png",
            };

            let filename = format!("dyson_gen_{random_suffix:016x}_{i}.{extension}");
            let path = std::env::temp_dir().join(&filename);

            if let Err(e) = std::fs::write(&path, &image.data) {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "failed to write generated image"
                );
                return Ok(ToolOutput::error(format!(
                    "failed to save generated image: {e}"
                )));
            }

            output = output.with_file(&path);
        }

        Ok(output)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::io::Cursor;

    // -- Mock provider -------------------------------------------------------

    struct MockImageProvider {
        images: Vec<GeneratedImage>,
    }

    #[async_trait]
    impl ImageGenerationProvider for MockImageProvider {
        async fn generate(
            &self,
            _prompt: &str,
            count: usize,
            _resolution: &str,
        ) -> Result<Vec<GeneratedImage>> {
            Ok(self
                .images
                .iter()
                .take(count)
                .map(|img| GeneratedImage {
                    data: img.data.clone(),
                    mime_type: img.mime_type.clone(),
                })
                .collect())
        }
    }

    /// A provider that ignores `count` and always returns all images —
    /// simulates the Gemini API returning extra inlineData parts.
    struct GreedyImageProvider {
        images: Vec<GeneratedImage>,
    }

    #[async_trait]
    impl ImageGenerationProvider for GreedyImageProvider {
        async fn generate(
            &self,
            _prompt: &str,
            _count: usize,
            _resolution: &str,
        ) -> Result<Vec<GeneratedImage>> {
            Ok(self
                .images
                .iter()
                .map(|img| GeneratedImage {
                    data: img.data.clone(),
                    mime_type: img.mime_type.clone(),
                })
                .collect())
        }
    }

    fn mock_tool(images: Vec<GeneratedImage>) -> ImageGenerateTool {
        ImageGenerateTool::new(Arc::new(MockImageProvider { images }))
    }

    fn greedy_mock_tool(images: Vec<GeneratedImage>) -> ImageGenerateTool {
        ImageGenerateTool::new(Arc::new(GreedyImageProvider { images }))
    }

    /// Create minimal valid PNG bytes for testing.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::new(1, 1);
        let mut buf = Vec::new();
        let mut cursor = Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    // -- Tests ---------------------------------------------------------------

    #[tokio::test]
    async fn empty_prompt_returns_error() {
        let tool = mock_tool(vec![]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool.run(&json!({"prompt": ""}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }

    #[tokio::test]
    async fn generates_single_image() {
        let tool = mock_tool(vec![GeneratedImage {
            data: tiny_png(),
            mime_type: "image/png".into(),
        }]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool
            .run(&json!({"prompt": "a sunset"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.files.len(), 1);
        assert!(result.content.contains("Generated 1 image(s)"));
        // Verify the file was written.
        assert!(result.files[0].exists());
        // Clean up.
        let _ = std::fs::remove_file(&result.files[0]);
    }

    #[tokio::test]
    async fn generates_multiple_images() {
        let tool = mock_tool(vec![
            GeneratedImage {
                data: tiny_png(),
                mime_type: "image/png".into(),
            },
            GeneratedImage {
                data: tiny_png(),
                mime_type: "image/png".into(),
            },
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool
            .run(&json!({"prompt": "test", "count": 2}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.files.len(), 2);
        assert!(result.content.contains("Generated 2 image(s)"));
        // Clean up.
        for f in &result.files {
            let _ = std::fs::remove_file(f);
        }
    }

    #[tokio::test]
    async fn prompt_too_long_returns_error() {
        let tool = mock_tool(vec![]);
        let ctx = ToolContext::from_cwd().unwrap();
        let long_prompt = "a".repeat(MAX_PROMPT_LEN + 1);
        let result = tool
            .run(&json!({"prompt": long_prompt}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("too long"));
    }

    #[test]
    fn tool_schema_has_required_prompt() {
        let tool = mock_tool(vec![]);
        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "prompt"));
    }

    #[test]
    fn tool_name_is_image_generate() {
        let tool = mock_tool(vec![]);
        assert_eq!(tool.name(), "image_generate");
    }

    #[test]
    fn openrouter_response_with_inline_data_url_is_decoded() {
        // Smallest plausible OpenRouter chat-completions response with
        // an image attached: choices[0].message.images[].image_url.url
        // is a data URL containing the same single-pixel PNG bytes the
        // other tests use.  extend_with_openrouter_images must decode
        // the base64 payload and surface it as a GeneratedImage.
        let png = tiny_png();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let resp = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "images": [
                        { "image_url": { "url": format!("data:image/png;base64,{b64}") } }
                    ]
                }
            }]
        });
        let mut out = Vec::new();
        extend_with_openrouter_images(&mut out, &resp).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mime_type, "image/png");
        assert_eq!(out[0].data, png);
    }

    #[test]
    fn openrouter_response_without_images_is_a_noop() {
        // Text-only completion: nothing to decode, no error.  Caller
        // accumulates an empty Vec and surfaces "no images" upstream.
        let resp = json!({
            "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
        });
        let mut out = Vec::new();
        extend_with_openrouter_images(&mut out, &resp).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn openrouter_response_skips_non_data_urls() {
        // A future model might return an https URL instead of a data
        // URL.  We don't fetch those (would punch the swarm proxy auth
        // boundary); they're silently skipped so callers fall through
        // to the "no images" error path.
        let resp = json!({
            "choices": [{
                "message": {
                    "images": [
                        { "image_url": { "url": "https://cdn.example/foo.png" } }
                    ]
                }
            }]
        });
        let mut out = Vec::new();
        extend_with_openrouter_images(&mut out, &resp).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn factory_creates_openrouter_provider_with_explicit_model_override() {
        // The factory uses the model_override when one is supplied —
        // crucial for `dyson swarm`, where the chat provider's default
        // model is the user's chat pick (e.g. claude-sonnet-4) but the
        // image_generate tool must always route to the image-capable
        // model id.  We can't easily downcast through dyn Trait, so
        // exercise the construction path directly: an unsupported
        // variant is rejected, OpenRouter succeeds.
        let or_config = ProviderConfig {
            provider_type: LlmProvider::OpenRouter,
            models: vec!["openrouter-default-chat-model".into()],
            api_key: crate::auth::Credential::from("sk-or-test".to_string()),
            base_url: Some("https://swarm.example/llm/openrouter".into()),
        };
        assert!(create_provider(&or_config, Some("google/gemini-3-pro-image-preview")).is_ok());

        let unsupported = ProviderConfig {
            provider_type: LlmProvider::Anthropic,
            models: vec!["claude".into()],
            api_key: crate::auth::Credential::from("sk-ant".to_string()),
            base_url: None,
        };
        // The trait object isn't Debug, so unwrap_err panics on the
        // bound — match by hand instead.
        let err = match create_provider(&unsupported, None) {
            Ok(_) => panic!("anthropic must not be wired for image generation"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("does not support image generation"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("openrouter"),
            "error must list openrouter as supported: {msg}"
        );
    }

    #[tokio::test]
    async fn provider_returning_extra_images_is_capped_to_count() {
        // Simulate a provider (like Gemini) that returns 3 images even
        // though only 1 was requested — the tool should cap to count.
        let tool = greedy_mock_tool(vec![
            GeneratedImage {
                data: tiny_png(),
                mime_type: "image/png".into(),
            },
            GeneratedImage {
                data: tiny_png(),
                mime_type: "image/png".into(),
            },
            GeneratedImage {
                data: tiny_png(),
                mime_type: "image/png".into(),
            },
        ]);
        let ctx = ToolContext::from_cwd().unwrap();
        let result = tool.run(&json!({"prompt": "a robot"}), &ctx).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.files.len(), 1, "should only send 1 image, not 3");
        assert!(result.content.contains("Generated 1 image(s)"));
        // Clean up.
        for f in &result.files {
            let _ = std::fs::remove_file(f);
        }
    }
}
