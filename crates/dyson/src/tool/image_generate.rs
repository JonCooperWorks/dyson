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

            let resp_body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| {
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
// Factory — create an ImageGenerationProvider from a ProviderConfig.
// ---------------------------------------------------------------------------

/// Create an `ImageGenerationProvider` from a provider configuration.
///
/// Dispatches on `provider_type` to select the right backend.
/// Currently supported: `Gemini`.
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
        ref other => Err(DysonError::Config(format!(
            "provider type {other:?} does not support image generation (supported: gemini)"
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

    fn mock_tool(images: Vec<GeneratedImage>) -> ImageGenerateTool {
        ImageGenerateTool::new(Arc::new(MockImageProvider { images }))
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
}
