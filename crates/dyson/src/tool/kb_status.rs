// ===========================================================================
// KbStatusTool — knowledge base statistics and health overview.
//
// Reports file counts, sizes, and topic coverage for the kb/ directory.
// Lightweight — no LLM calls, just workspace enumeration.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use serde_json::json;

use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct KbStatusTool;

#[async_trait]
impl Tool for KbStatusTool {
    fn name(&self) -> &str {
        "kb_status"
    }

    fn description(&self) -> &str {
        "Show knowledge base statistics: file counts, sizes, and topic overview. \
         Use this to understand what's in the KB before searching or adding content."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn run(
        &self,
        _input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("kb_status")?;

        // Collect data under the lock, then format outside it.
        let (raw_files, raw_bytes, wiki_files, wiki_bytes, has_index) = {
            let ws = ws.read().await;
            let files = ws.list_files();

            let mut raw_bytes = 0usize;
            let mut wiki_bytes = 0usize;
            let mut raw_files: Vec<String> = Vec::with_capacity(files.len());
            let mut wiki_files: Vec<String> = Vec::with_capacity(files.len());

            for name in &files {
                if let Some(content) = ws.get(name) {
                    if name.starts_with("kb/raw/") {
                        raw_bytes += content.len();
                        raw_files.push(name.clone());
                    } else if name.starts_with("kb/wiki/") {
                        wiki_bytes += content.len();
                        wiki_files.push(name.clone());
                    }
                }
            }

            let has_index = ws.get("kb/INDEX.md").is_some();
            (raw_files, raw_bytes, wiki_files, wiki_bytes, has_index)
        };

        let mut output = String::from("## Knowledge Base Status\n\n");

        writeln!(
            &mut output,
            "- **Raw sources:** {} file(s), {}",
            raw_files.len(),
            format_bytes(raw_bytes)
        )
        .unwrap();
        writeln!(
            &mut output,
            "- **Wiki articles:** {} file(s), {}",
            wiki_files.len(),
            format_bytes(wiki_bytes)
        )
        .unwrap();
        writeln!(
            &mut output,
            "- **INDEX.md:** {}",
            if has_index {
                "present"
            } else {
                "not yet created"
            }
        )
        .unwrap();

        if !raw_files.is_empty() {
            output.push_str("\n### Raw Sources\n");
            for f in &raw_files {
                writeln!(&mut output, "- {f}").unwrap();
            }
        }

        if !wiki_files.is_empty() {
            output.push_str("\n### Wiki Articles\n");
            for f in &wiki_files {
                writeln!(&mut output, "- {f}").unwrap();
            }
        }

        if raw_files.is_empty() && wiki_files.is_empty() {
            output.push_str(
                "\nThe knowledge base is empty. Use `workspace` (op=\"update\") to add files \
                 under `kb/raw/` (source material) or `kb/wiki/` (articles).",
            );
        }

        Ok(ToolOutput::success(output))
    }
}

pub fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_bytes() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_kb() {
        assert_eq!(format_bytes(2048), "2.0 KB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MB");
    }
}
