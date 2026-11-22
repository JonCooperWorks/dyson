// ===========================================================================
// Office document processing — extract text from docx, xlsx, pptx via undoc.
//
// Extracts structured content as Markdown, which is ideal for LLM consumption:
// headings, lists, tables, and formatting are preserved semantically.  The raw
// bytes are NOT base64-encoded (unlike PDFs) because no LLM provider has native
// Office document input — Markdown text is the only representation we need.
// ===========================================================================

use dyson_core::message::ContentBlock;

/// Process raw Office document bytes into a `ContentBlock::Text`.
///
/// Supports docx, xlsx, and pptx.  Extracts content as Markdown via `undoc`.
/// The `file_name` is used purely for labelling in the prompt.
pub fn process_office(
    data: &[u8],
    file_name: Option<&str>,
) -> dyson_core::error::Result<ContentBlock> {
    let doc = undoc::parse_bytes(data).map_err(|e| {
        dyson_core::error::DysonError::Config(format!("failed to parse Office document: {e}"))
    })?;

    let options = undoc::render::RenderOptions::default();
    let markdown = undoc::render::to_markdown(&doc, &options).map_err(|e| {
        dyson_core::error::DysonError::Config(format!(
            "failed to render Office document as markdown: {e}"
        ))
    })?;

    if markdown.trim().is_empty() {
        return Err(dyson_core::error::DysonError::Config(
            "Office document appears to be empty — no text content extracted".into(),
        ));
    }

    let label = file_name.unwrap_or("document");
    tracing::debug!(
        file_name = label,
        extracted_chars = markdown.len(),
        extracted_words = markdown.split_whitespace().count(),
        "Office document processed successfully"
    );

    let text = format!("=== file: {label} ===\n{markdown}");
    Ok(ContentBlock::Text { text })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage_bytes() {
        let result = process_office(b"not a real office file", Some("bad.docx"));
        assert!(result.is_err());
    }
}
