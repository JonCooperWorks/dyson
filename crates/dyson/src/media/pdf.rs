// ===========================================================================
// PDF processing — extract text and base64-encode for document-capable models.
//
// Dual-path design:
//
//   1. Providers that support native PDF input (Anthropic) get the raw
//      base64-encoded PDF as a `document` content block — best fidelity.
//   2. Providers that don't (OpenAI-compat, OpenRouter, local models) get
//      the extracted text as a `text` content block — saves tokens and
//      works everywhere.
//
// The media resolver populates both fields in ContentBlock::Document so
// each LLM client can choose the best representation at serialization time.
// ===========================================================================

use base64::Engine;

use crate::message::ContentBlock;

/// Maximum PDF file size we'll process (32 MB — Anthropic's limit).
const MAX_PDF_BYTES: usize = 32 * 1024 * 1024;

/// Process raw PDF bytes into a ContentBlock::Document.
///
/// Extracts text as Markdown via `unpdf` and base64-encodes the raw PDF.
/// Both are stored in the resulting content block so each LLM provider
/// can pick the best representation.
pub fn process_pdf(data: &[u8]) -> crate::Result<ContentBlock> {
    if data.len() > MAX_PDF_BYTES {
        return Err(crate::DysonError::Config(format!(
            "PDF too large: {:.1} MB (max {:.0} MB)",
            data.len() as f64 / (1024.0 * 1024.0),
            MAX_PDF_BYTES as f64 / (1024.0 * 1024.0),
        )));
    }

    // Parse and extract text as Markdown for providers that can't handle
    // native PDFs.  Markdown preserves headings, lists, and tables.
    let doc = unpdf::parse_bytes(data)
        .map_err(|e| crate::DysonError::Config(format!("failed to parse PDF: {e}")))?;
    let options = unpdf::render::RenderOptions::default();
    let extracted_text = unpdf::render::to_markdown(&doc, &options)
        .map_err(|e| crate::DysonError::Config(format!("failed to render PDF as markdown: {e}")))?;

    // Base64-encode the raw PDF for providers that support native documents.
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);

    tracing::debug!(
        pdf_bytes = data.len(),
        base64_bytes = b64.len(),
        extracted_chars = extracted_text.len(),
        extracted_words = extracted_text.split_whitespace().count(),
        "PDF processed successfully"
    );

    Ok(ContentBlock::Document {
        data: b64,
        extracted_text,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_pdf() {
        let data = vec![0u8; MAX_PDF_BYTES + 1];
        let result = process_pdf(&data);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_invalid_pdf() {
        let result = process_pdf(b"not a pdf");
        assert!(result.is_err());
    }

    #[test]
    fn extracts_text_from_valid_pdf() {
        let pdf_bytes = minimal_pdf_with_text("Hello Dyson");
        let block = process_pdf(&pdf_bytes).unwrap();
        match block {
            ContentBlock::Document {
                data,
                extracted_text,
            } => {
                // Verify base64 is valid.
                base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .expect("should be valid base64");
                assert!(
                    extracted_text.contains("Hello Dyson"),
                    "extracted text should contain our string, got: {extracted_text}"
                );
            }
            other => panic!("expected Document block, got: {other:?}"),
        }
    }

    /// Build a minimal valid PDF with extractable text.
    ///
    /// Computes exact byte offsets for the xref table so the parser can
    /// properly parse the file and extract text.
    fn minimal_pdf_with_text(text: &str) -> Vec<u8> {
        let page_content = format!("BT /F1 12 Tf 100 700 Td ({text}) Tj ET");
        let stream_len = page_content.len();

        let header = b"%PDF-1.4\n";

        // Build each object as bytes so we can track exact offsets.
        let obj1 = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2 = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n";
        let obj3 = b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n";
        let obj4 = format!(
            "4 0 obj\n<< /Length {stream_len} >>\nstream\n{page_content}\nendstream\nendobj\n"
        );
        let obj4 = obj4.as_bytes();
        let obj5 = b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n";

        // Compute exact byte offsets.
        let off1 = header.len();
        let off2 = off1 + obj1.len();
        let off3 = off2 + obj2.len();
        let off4 = off3 + obj3.len();
        let off5 = off4 + obj4.len();

        let mut body = Vec::new();
        body.extend_from_slice(header);
        body.extend_from_slice(obj1);
        body.extend_from_slice(obj2);
        body.extend_from_slice(obj3);
        body.extend_from_slice(obj4);
        body.extend_from_slice(obj5);

        let xref_offset = body.len();

        let xref = format!(
            "xref\n0 6\n\
             0000000000 65535 f \n\
             {off1:010} 00000 n \n\
             {off2:010} 00000 n \n\
             {off3:010} 00000 n \n\
             {off4:010} 00000 n \n\
             {off5:010} 00000 n \n"
        );

        body.extend_from_slice(xref.as_bytes());
        body.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );

        body
    }
}
