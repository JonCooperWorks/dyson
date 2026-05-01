// ===========================================================================
// ReadFile tool — read file contents with optional line range or AST-aware
// symbol extraction.
//
// Three modes:
//   - default: returns the whole file (or `offset`/`limit` slice).
//   - PDF: text is extracted automatically from `.pdf` files.
//   - symbol mode: when `symbol` is set, the file is parsed with tree-sitter
//     and only definitions whose name matches are returned, with their
//     source spans.  An optional `symbol_kind` filter narrows by kind
//     (`function`, `struct`, `class`, ...).  Symbol mode is supported on
//     the same 19 grammars as `bulk_edit`.
// ===========================================================================

use std::fmt::Write as _;

use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;

use crate::ast;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::truncate_output;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns lines with line numbers. \
         Use `offset` (1-based line number) and `limit` (number of lines) \
         to read a specific range. PDF files are automatically detected \
         and their text content is extracted. \
         Set `symbol` to extract one definition (function/class/struct/...) \
         from a source file via tree-sitter — returns just that definition's \
         source instead of the whole file. Optional `symbol_kind` filters by \
         kind (e.g. 'function', 'struct', 'class') when the same name has \
         multiple definitions. Symbol mode supports Rust, Python, JS/TS/TSX, \
         Go, Java, C, C++, C#, Ruby, Kotlin, Swift, Zig, Elixir, Erlang, \
         OCaml, Haskell, Nix, JSON."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file (relative to working directory or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start reading from (default: 1). Ignored when `symbol` is set."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default: all). Ignored when `symbol` is set."
                },
                "symbol": {
                    "type": "string",
                    "description": "If set, returns the source of the named definition(s) in this file (AST-based, tree-sitter). Overrides offset/limit. Errors if the file has no supported grammar."
                },
                "symbol_kind": {
                    "type": "string",
                    "description": "Optional kind filter for `symbol` (e.g. 'function', 'struct', 'class', 'interface', 'trait', 'method', 'type'). Matches the cleaned tree-sitter kind."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or_else(|| DysonError::tool("read_file", "missing or invalid 'file_path'"))?;

        let path = match ctx.resolve_path(file_path) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };

        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().map(|l| l as usize);

        // Symbol mode short-circuits everything else: parse the file with
        // tree-sitter, extract the matching definition(s), return just that
        // source.  Done up-front so we never touch line streaming or the PDF
        // path when the agent only wants one function out of a 5k-line file.
        if let Some(symbol) = input["symbol"].as_str() {
            if symbol.is_empty() {
                return Ok(ToolOutput::error("symbol must not be empty"));
            }
            let kind = input["symbol_kind"].as_str().map(str::to_string);
            let symbol = symbol.to_string();
            let working_dir = ctx.working_dir.clone();
            let path_clone = path.clone();
            return tokio::task::spawn_blocking(move || {
                extract_symbol(&path_clone, &working_dir, &symbol, kind.as_deref())
            })
            .await
            .map_err(|e| DysonError::tool("read_file", format!("symbol task failed: {e}")))?;
        }

        // Guard against reading very large files into memory.
        const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                return Ok(ToolOutput::error(format!(
                    "file '{}' is too large ({:.1} MB, limit is {:.0} MB)",
                    path.display(),
                    meta.len() as f64 / (1024.0 * 1024.0),
                    MAX_FILE_SIZE as f64 / (1024.0 * 1024.0),
                )));
            }
            Err(e) => {
                return Ok(ToolOutput::error(super::path_err("stat", &path, e)));
            }
            _ => {}
        }

        // PDF files: extract text as Markdown instead of reading raw binary.
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            return read_document_as_markdown(&path, extract_pdf_text).await;
        }

        // Office documents (docx, xlsx, pptx): extract as Markdown.
        if path.extension().is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().to_str().unwrap_or(""),
                "docx" | "xlsx" | "pptx"
            )
        }) {
            return read_document_as_markdown(&path, extract_office_text).await;
        }

        // Stream line-by-line with skip/take so that large files with small
        // offset/limit ranges don't need to be read entirely into memory.
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "cannot read '{}': {e}",
                    path.display()
                )));
            }
        };

        let reader = tokio::io::BufReader::new(file);
        let mut lines = reader.lines();
        let start = offset - 1;

        // Skip lines before the requested offset.
        for _ in 0..start {
            match lines.next_line().await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    return Ok(ToolOutput::error(super::path_err("read", &path, e)));
                }
            }
        }

        // Read the requested range.
        let max_lines = limit.unwrap_or(usize::MAX);
        let mut output = String::new();
        let mut raw_lines: Vec<String> = Vec::new();
        let mut count = 0;
        while count < max_lines {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line_num = start + count + 1;
                    let _ = writeln!(output, "{line_num:>6}\t{line}");
                    raw_lines.push(line);
                    count += 1;
                }
                Ok(None) => break,
                Err(e) => {
                    return Ok(ToolOutput::error(super::path_err("read", &path, e)));
                }
            }
        }

        // Truncate if too large.
        let output = truncate_output(&output);

        if output.is_empty() {
            return Ok(ToolOutput::success("(empty file)"));
        }

        let view = crate::tool::view::ToolView::Read {
            path: path.display().to_string(),
            lines: raw_lines,
            highlight: None,
        };
        Ok(ToolOutput::success(output).with_view(view))
    }
}

/// Parse `path` with tree-sitter and return the source of every definition
/// matching `name` (and optionally `kind`).  Errors clearly when the file
/// has no supported grammar — symbol mode is AST-only on purpose; falling
/// back to a text grep would silently lie about precision.
fn extract_symbol(
    path: &std::path::Path,
    working_dir: &std::path::Path,
    name: &str,
    kind: Option<&str>,
) -> Result<ToolOutput> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let parsed = match ast::try_parse_file(path, &working_dir_canon, false)? {
        Some(pair) => pair,
        None => {
            return Ok(ToolOutput::error(format!(
                "symbol mode: no tree-sitter grammar for '{}' (or file is too large / not UTF-8)",
                path.display()
            )));
        }
    };
    let (config, parsed_file) = parsed;

    let matches = ast::find_definitions_by_name(&parsed_file, config, name, kind);

    if matches.is_empty() {
        let kind_part = kind.map(|k| format!(" of kind '{k}'")).unwrap_or_default();
        return Ok(ToolOutput::success(format!(
            "(no definition named '{name}'{kind_part} in {})",
            parsed_file.rel_path
        )));
    }

    // Render each match with its kind, line, and source span.  Multiple hits
    // get blank-line separators so the agent can read them as a single block.
    let mut out = String::new();
    for (i, m) in matches.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let _ = writeln!(out, "// {}:{} ({})", parsed_file.rel_path, m.line, m.kind);
        out.push_str(&parsed_file.source[m.start_byte..m.end_byte]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    let truncated = truncate_output(&out);
    Ok(ToolOutput::success(truncated))
}

/// Extract PDF text as Markdown via unpdf.
fn extract_pdf_text(data: &[u8]) -> std::result::Result<String, String> {
    let doc = unpdf::parse_bytes(data).map_err(|e| e.to_string())?;
    let opts = unpdf::render::RenderOptions::default();
    unpdf::render::to_markdown(&doc, &opts).map_err(|e| e.to_string())
}

/// Extract Office document text as Markdown via undoc.
fn extract_office_text(data: &[u8]) -> std::result::Result<String, String> {
    let doc = undoc::parse_bytes(data).map_err(|e| e.to_string())?;
    let opts = undoc::render::RenderOptions::default();
    undoc::render::to_markdown(&doc, &opts).map_err(|e| e.to_string())
}

/// Decompression-bomb guards for office/PDF extraction.
///
/// A small compressed input can expand to hundreds of megabytes during
/// PDF stream decoding or Office zip inflation.  The upstream 50 MB
/// raw-byte cap only bounds the file on disk — not the text the parser
/// produces.  These two constants keep the extracted text bounded in
/// both absolute size and relative blow-up.
const MAX_EXTRACTED_BYTES: usize = 16 * 1024 * 1024;
/// Maximum ratio of extracted-bytes to compressed-bytes before we
/// treat the file as a decompression bomb and refuse.  50 covers
/// well-compressed-but-legitimate office docs (text-heavy slide decks,
/// spreadsheets with lots of shared strings); anything higher is almost
/// always adversarial.
const MAX_EXTRACT_RATIO: usize = 50;
/// Ratio check only kicks in once the compressed size is above this
/// floor, so tiny inputs (a 200-byte pptx stub that expands to 15 KB)
/// don't trip on a ratio technicality.
const RATIO_MIN_INPUT_BYTES: usize = 64 * 1024;

/// Read a binary document (PDF, Office, etc.) from disk, extract Markdown via
/// the provided `extractor`, and return the result as a tool output.
async fn read_document_as_markdown<E>(
    path: &std::path::Path,
    extractor: E,
) -> crate::Result<ToolOutput>
where
    E: FnOnce(&[u8]) -> std::result::Result<String, String>,
{
    let data = match tokio::fs::read(path).await {
        Ok(d) => d,
        Err(e) => {
            return Ok(ToolOutput::error(super::path_err("read", path, e)));
        }
    };

    let input_len = data.len();
    let text = match extractor(&data) {
        Ok(t) => t,
        Err(e) => {
            return Ok(ToolOutput::error(format!(
                "failed to extract text from '{}': {e}",
                path.display()
            )));
        }
    };

    if text.len() > MAX_EXTRACTED_BYTES {
        return Ok(ToolOutput::error(format!(
            "document '{}' expanded to {:.1} MB of text (limit {} MB) — refusing to process",
            path.display(),
            text.len() as f64 / (1024.0 * 1024.0),
            MAX_EXTRACTED_BYTES / (1024 * 1024),
        )));
    }
    if input_len >= RATIO_MIN_INPUT_BYTES && text.len() / input_len.max(1) >= MAX_EXTRACT_RATIO {
        return Ok(ToolOutput::error(format!(
            "document '{}' decompression ratio {}x exceeds limit {}x — refusing to process",
            path.display(),
            text.len() / input_len.max(1),
            MAX_EXTRACT_RATIO,
        )));
    }

    if text.trim().is_empty() {
        return Ok(ToolOutput::success(
            "(document contains no extractable text)",
        ));
    }

    let output = truncate_output(&text);
    Ok(ToolOutput::success(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use std::io::Write;

    #[tokio::test]
    async fn read_simple_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        writeln!(f, "line one").unwrap();
        writeln!(f, "line two").unwrap();
        writeln!(f, "line three").unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "test.txt"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("line one"));
        assert!(output.content.contains("line three"));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        let mut f = std::fs::File::create(&file).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {i}").unwrap();
        }

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "test.txt", "offset": 3, "limit": 2});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("line 3"));
        assert!(output.content.contains("line 4"));
        assert!(!output.content.contains("line 5"));
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "nope.txt"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
    }

    #[test]
    fn is_agent_only() {
        assert!(ReadFileTool.agent_only());
    }

    // -----------------------------------------------------------------------
    // Symbol mode
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn symbol_extracts_function_rust() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn alpha() -> i32 { 1 }\n\n\
             fn target() -> i32 {\n    let x = 42;\n    x + 1\n}\n\n\
             fn beta() {}\n",
        )
        .unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "lib.rs", "symbol": "target"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("fn target()"));
        assert!(output.content.contains("x + 1"));
        // Other definitions must NOT leak into the output.
        assert!(!output.content.contains("fn alpha"));
        assert!(!output.content.contains("fn beta"));
    }

    #[tokio::test]
    async fn symbol_extracts_method_inside_impl() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "struct Foo;\nimpl Foo {\n    fn outer() {}\n    fn target(&self) -> i32 { 7 }\n}\n",
        )
        .unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "lib.rs", "symbol": "target"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("fn target(&self)"));
    }

    #[tokio::test]
    async fn symbol_kind_filter_disambiguates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn target() {}\nstruct target { x: i32 }\n",
        )
        .unwrap();

        let tool = ReadFileTool;

        // No filter: both match, both rendered.
        let output = tool
            .run(
                &serde_json::json!({"file_path": "lib.rs", "symbol": "target"}),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.content.contains("fn target()"));
        assert!(output.content.contains("struct target"));

        // Kind filter: only the struct.
        let output = tool
            .run(
                &serde_json::json!({
                    "file_path": "lib.rs",
                    "symbol": "target",
                    "symbol_kind": "struct",
                }),
                &ToolContext::for_test(tmp.path()),
            )
            .await
            .unwrap();
        assert!(output.content.contains("struct target"));
        assert!(!output.content.contains("fn target()"));
    }

    #[tokio::test]
    async fn symbol_no_match_message() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "lib.rs", "symbol": "ghost"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("no definition named 'ghost'"));
    }

    #[tokio::test]
    async fn symbol_unsupported_extension_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.md"), "# target\n").unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "notes.md", "symbol": "target"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("no tree-sitter grammar"));
    }

    #[tokio::test]
    async fn symbol_empty_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let tool = ReadFileTool;
        let input = serde_json::json!({"file_path": "lib.rs", "symbol": ""});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }
}
