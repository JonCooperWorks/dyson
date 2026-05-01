// ===========================================================================
// AstDescribeTool — parse a snippet or file range and return its tree-sitter
// node tree.  Use before writing a non-trivial ast_query: look up the actual
// node kinds and field names instead of guessing.
//
// Two modes:
//   - snippet + language  — parse a hypothetical (e.g. "does `foo?.bar()`
//     parse the same as `foo.bar()` in TypeScript?").
//   - path + optional line_range  — parse real code, optionally narrowed to
//     the subtree(s) that overlap a line range.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use tree_sitter::Node;

use crate::ast;
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::MAX_OUTPUT_BYTES;

/// Cap on tree depth rendered.  A real file's root can easily be 20+ levels
/// deep; most structural questions are answered in the first 8.
const DEFAULT_MAX_DEPTH: usize = 12;

/// Cap on leaf-node text rendered inline — identifiers and strings are
/// useful, but a 400-byte template literal wastes tokens.
const LEAF_TEXT_LIMIT: usize = 64;

pub struct AstDescribeTool;

#[async_trait]
impl Tool for AstDescribeTool {
    fn name(&self) -> &str {
        "ast_describe"
    }

    fn description(&self) -> &str {
        "Parse a snippet or file range and return its tree-sitter node tree \
         (kinds + field names + leaf text).  Use this before writing a \
         non-trivial ast_query — look up the real grammar instead of guessing \
         node names.  Two modes: pass `snippet` + `language` to parse a \
         hypothetical; pass `path` with optional `line_range` to parse real \
         code.  Supports the same 20 languages as ast_query."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "snippet": {
                    "type": "string",
                    "description": "Source code to parse.  Requires `language`.  \
                        Use for hypotheticals: `foo?.bar()`, `await x.y()`, a \
                        decorator shape you haven't seen in the codebase yet."
                },
                "language": {
                    "type": "string",
                    "description": "Target language.  Required with `snippet`; \
                        inferred from `path` extension otherwise.  Same set as \
                        ast_query: rust, python, javascript, typescript, tsx, \
                        go, java, c, cpp, csharp, ruby, kotlin, swift, zig, \
                        elixir, erlang, ocaml, haskell, nix, json (+ aliases)."
                },
                "path": {
                    "type": "string",
                    "description": "File to parse (relative to working directory).  \
                        Mutually exclusive with `snippet`."
                },
                "line_range": {
                    "type": "string",
                    "description": "Optional 1-indexed inclusive line range \
                        (e.g. '42-58' or '42') to narrow output to subtrees \
                        overlapping those lines.  Only valid with `path`."
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Max tree depth to render.  Default 12."
                }
            }
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let snippet = input["snippet"].as_str();
        let path = input["path"].as_str();

        if snippet.is_some() && path.is_some() {
            return Ok(ToolOutput::error(
                "pass either `snippet` or `path`, not both",
            ));
        }
        if snippet.is_none() && path.is_none() {
            return Ok(ToolOutput::error(
                "provide either `snippet` (with `language`) or `path`",
            ));
        }

        let max_depth = input["max_depth"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_DEPTH);

        let (source, config, header) = match (snippet, path) {
            (Some(s), _) => {
                let lang = input["language"].as_str().ok_or_else(|| {
                    DysonError::tool("ast_describe", "snippet mode requires `language`")
                })?;
                let cfg = match ast::config_for_language_name(lang) {
                    Some(c) => c,
                    None => {
                        return Ok(ToolOutput::error(format!(
                            "unknown language '{lang}'.  Supported: rust, python, \
                             javascript, typescript, tsx, go, java, c, cpp, csharp, \
                             ruby, kotlin, swift, zig, elixir, erlang, ocaml, haskell, \
                             nix, json"
                        )));
                    }
                };
                (s.to_string(), cfg, format!("<snippet> ({lang})"))
            }
            (None, Some(p)) => {
                let resolved = match ctx.resolve_path(p) {
                    Ok(r) => r,
                    Err(e) => return Ok(e),
                };
                if !resolved.exists() {
                    return Ok(ToolOutput::error(format!(
                        "file does not exist: '{}'",
                        resolved.display()
                    )));
                }
                if !resolved.is_file() {
                    return Ok(ToolOutput::error(format!(
                        "path is not a file: '{}'",
                        resolved.display()
                    )));
                }

                let src = match std::fs::read_to_string(&resolved) {
                    Ok(s) => s,
                    Err(e) => {
                        return Ok(ToolOutput::error(format!("read failed: {e}")));
                    }
                };

                let cfg = match input["language"].as_str() {
                    Some(name) => match ast::config_for_language_name(name) {
                        Some(c) => c,
                        None => {
                            return Ok(ToolOutput::error(format!("unknown language '{name}'")));
                        }
                    },
                    None => {
                        let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
                        match ast::config_for_extension(ext) {
                            Some(c) => c,
                            None => {
                                return Ok(ToolOutput::error(format!(
                                    "couldn't infer language from extension '.{ext}' — \
                                     pass `language` explicitly"
                                )));
                            }
                        }
                    }
                };

                (src, cfg, p.to_string())
            }
            (None, None) => unreachable!("guarded above"),
        };

        let line_range = match input["line_range"].as_str() {
            Some(s) => match parse_line_range(s) {
                Ok(r) => Some(r),
                Err(msg) => return Ok(ToolOutput::error(msg)),
            },
            None => None,
        };

        if snippet.is_some() && line_range.is_some() {
            return Ok(ToolOutput::error(
                "`line_range` only makes sense with `path`",
            ));
        }

        let rendered = match describe_source(&source, config, line_range, max_depth) {
            Ok(s) => s,
            Err(e) => return Ok(ToolOutput::error(e)),
        };

        let mut out = String::new();
        writeln!(&mut out, "# {header}").unwrap();
        if let Some((s, e)) = line_range {
            writeln!(&mut out, "# line range: {s}-{e}").unwrap();
        }
        writeln!(&mut out, "# max_depth: {max_depth}").unwrap();
        out.push('\n');
        out.push_str(&rendered);

        Ok(ToolOutput::success(out))
    }
}

/// Parse `source` as `config.language` and render its tree-sitter node tree.
///
/// This is the engine of the `ast_describe` tool, exposed for direct use
/// by smoke tests and other callers that want a rendered AST without
/// going through the Tool/ToolContext plumbing.
///
/// When `line_range` is `Some((start, end))` (1-indexed, inclusive), output
/// is narrowed to the smallest named subtrees that fully contain the range.
/// `max_depth` caps the rendered depth; deeper nodes are replaced with a
/// `... (N children truncated)` marker.  Returned string is capped at
/// `MAX_OUTPUT_BYTES` with a trailing truncation note.
pub fn describe_source(
    source: &str,
    config: &'static crate::ast::LanguageConfig,
    line_range: Option<(usize, usize)>,
    max_depth: usize,
) -> std::result::Result<String, String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&config.language)
        .map_err(|e| format!("parser setup: {e}"))?;

    let tree = parser.parse(source, None).ok_or_else(|| {
        "parser returned no tree — source may be too large or malformed".to_string()
    })?;

    let roots: Vec<Node<'_>> = match line_range {
        Some((start_line, end_line)) => {
            nodes_overlapping_lines(tree.root_node(), start_line, end_line)
        }
        None => vec![tree.root_node()],
    };

    if roots.is_empty() {
        return Ok("(no nodes overlap the requested line range)".to_string());
    }

    let mut out = String::new();
    for (i, root) in roots.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_node(*root, source, 0, max_depth, &mut out);
        if out.len() >= MAX_OUTPUT_BYTES {
            out.push_str("\n... (truncated at output byte cap)\n");
            break;
        }
    }
    Ok(out)
}

/// Parse a `start-end` or single-line range.  1-indexed, inclusive.  Returns
/// `(start, end)` in 1-indexed form on success.
fn parse_line_range(s: &str) -> std::result::Result<(usize, usize), String> {
    let trimmed = s.trim();
    match trimmed.split_once('-') {
        Some((a, b)) => {
            let start: usize = a
                .trim()
                .parse()
                .map_err(|_| format!("invalid line_range start: '{a}'"))?;
            let end: usize = b
                .trim()
                .parse()
                .map_err(|_| format!("invalid line_range end: '{b}'"))?;
            if start == 0 || end == 0 {
                return Err("line_range is 1-indexed; 0 is not valid".into());
            }
            if start > end {
                return Err(format!(
                    "line_range start {start} is greater than end {end}"
                ));
            }
            Ok((start, end))
        }
        None => {
            let n: usize = trimmed
                .parse()
                .map_err(|_| format!("invalid line_range: '{trimmed}'"))?;
            if n == 0 {
                return Err("line_range is 1-indexed; 0 is not valid".into());
            }
            Ok((n, n))
        }
    }
}

/// Find the smallest subtrees that overlap the requested line range.  We
/// prefer named nodes at the top of each overlap — rendering the whole
/// enclosing function when the range is just a few lines inside it is the
/// useful default.
fn nodes_overlapping_lines(root: Node<'_>, start_line: usize, end_line: usize) -> Vec<Node<'_>> {
    // Convert to 0-indexed rows.
    let start_row = start_line - 1;
    let end_row = end_line - 1;

    let mut out = Vec::new();
    collect_overlapping(root, start_row, end_row, &mut out);
    out
}

fn collect_overlapping<'a>(
    node: Node<'a>,
    start_row: usize,
    end_row: usize,
    out: &mut Vec<Node<'a>>,
) {
    let node_start = node.start_position().row;
    let node_end = node.end_position().row;

    if node_end < start_row || node_start > end_row {
        return;
    }

    // If any named child fully contains the range, recurse into it.
    // Otherwise, this node is the tightest fit and we render it.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let cs = child.start_position().row;
        let ce = child.end_position().row;
        if cs <= start_row && ce >= end_row {
            collect_overlapping(child, start_row, end_row, out);
            return;
        }
    }

    out.push(node);
}

/// Render a node and its descendants as an indented tree.
fn render_node(node: Node<'_>, source: &str, depth: usize, max_depth: usize, out: &mut String) {
    if out.len() >= MAX_OUTPUT_BYTES {
        return;
    }

    let indent = "  ".repeat(depth);
    let kind = node.kind();
    let start = node.start_position();
    let line_info = format!("L{}:{}", start.row + 1, start.column + 1);

    // Is this a leaf worth rendering text for?  Named nodes with no named
    // children are the interesting terminals (identifiers, strings,
    // primitive literals).
    let mut has_named_child = false;
    let mut named_children = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            has_named_child = true;
            named_children += 1;
        }
    }

    let is_leaf = !has_named_child;
    let text_preview = if is_leaf && node.is_named() {
        let slice = &source[node.start_byte()..node.end_byte().min(source.len())];
        Some(truncate_text(slice))
    } else {
        None
    };

    // Format: "  (call_expression)  [L12:5]"
    //         "    function: (identifier) \"foo\"  [L12:5]"
    write!(out, "{indent}").unwrap();
    if !node.is_named() {
        // Anonymous nodes (punctuation, keywords) are rendered as quoted
        // terminals — tree-sitter's own s-exp convention.
        let slice = &source[node.start_byte()..node.end_byte().min(source.len())];
        writeln!(out, "\"{}\"  [{line_info}]", escape_text(slice)).unwrap();
        return;
    }

    write!(out, "({kind}").unwrap();
    if let Some(text) = text_preview {
        write!(out, " {text:?}").unwrap();
    }
    write!(out, ")").unwrap();
    writeln!(out, "  [{line_info}]").unwrap();

    if depth + 1 >= max_depth && named_children > 0 {
        writeln!(
            out,
            "{indent}  ...  ({named_children} named children truncated at max_depth)"
        )
        .unwrap();
        return;
    }

    // Recurse, annotating field names.
    let mut cursor = node.walk();
    let mut child_idx = 0u32;
    for child in node.children(&mut cursor) {
        if !child.is_named() && !is_interesting_anonymous(child.kind()) {
            child_idx += 1;
            continue;
        }
        let field_name = node.field_name_for_child(child_idx);
        if let Some(name) = field_name {
            let field_indent = "  ".repeat(depth + 1);
            write!(out, "{field_indent}{name}: ").unwrap();
            render_node_inline(child, source, depth + 1, max_depth, out);
        } else {
            render_node(child, source, depth + 1, max_depth, out);
        }
        child_idx += 1;
        if out.len() >= MAX_OUTPUT_BYTES {
            return;
        }
    }
}

/// Render a child inline after a `field: ` prefix — no leading indent, but
/// descendants indent normally.
fn render_node_inline(
    node: Node<'_>,
    source: &str,
    depth: usize,
    max_depth: usize,
    out: &mut String,
) {
    let kind = node.kind();
    let start = node.start_position();
    let line_info = format!("L{}:{}", start.row + 1, start.column + 1);

    if !node.is_named() {
        let slice = &source[node.start_byte()..node.end_byte().min(source.len())];
        writeln!(out, "\"{}\"  [{line_info}]", escape_text(slice)).unwrap();
        return;
    }

    let mut cursor = node.walk();
    let mut named_children = 0usize;
    for child in node.children(&mut cursor) {
        if child.is_named() {
            named_children += 1;
        }
    }
    let is_leaf = named_children == 0;

    write!(out, "({kind}").unwrap();
    if is_leaf {
        let slice = &source[node.start_byte()..node.end_byte().min(source.len())];
        write!(out, " {:?}", truncate_text(slice)).unwrap();
    }
    write!(out, ")").unwrap();
    writeln!(out, "  [{line_info}]").unwrap();

    if is_leaf {
        return;
    }

    if depth + 1 >= max_depth {
        let ind = "  ".repeat(depth + 1);
        writeln!(
            out,
            "{ind}...  ({named_children} named children truncated at max_depth)"
        )
        .unwrap();
        return;
    }

    let mut cursor = node.walk();
    let mut child_idx = 0u32;
    for child in node.children(&mut cursor) {
        if !child.is_named() && !is_interesting_anonymous(child.kind()) {
            child_idx += 1;
            continue;
        }
        let field_name = node.field_name_for_child(child_idx);
        let child_indent = "  ".repeat(depth + 1);
        if let Some(name) = field_name {
            write!(out, "{child_indent}{name}: ").unwrap();
            render_node_inline(child, source, depth + 1, max_depth, out);
        } else {
            render_node(child, source, depth + 1, max_depth, out);
        }
        child_idx += 1;
        if out.len() >= MAX_OUTPUT_BYTES {
            return;
        }
    }
}

/// Truncate leaf text for inline display, collapsing newlines.
fn truncate_text(s: &str) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { '⏎' } else { c }).collect();
    if collapsed.len() <= LEAF_TEXT_LIMIT {
        collapsed
    } else {
        let mut truncated = String::with_capacity(LEAF_TEXT_LIMIT + 3);
        for ch in collapsed.chars() {
            if truncated.len() + ch.len_utf8() > LEAF_TEXT_LIMIT {
                break;
            }
            truncated.push(ch);
        }
        truncated.push_str("...");
        truncated
    }
}

fn escape_text(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// We render anonymous nodes for punctuation/keywords when they carry
/// structural meaning a reader would want (`async`, `await`, `unsafe`).
/// Pure delimiters like `(` `,` `;` are dropped to cut noise.
fn is_interesting_anonymous(kind: &str) -> bool {
    matches!(
        kind,
        "async" | "await" | "unsafe" | "mut" | "static" | "const" | "pub" | "extern"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn snippet_mode_parses_rust_call() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "fn f() { Command::new(\"ls\"); }",
            "language": "rust"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("call_expression"),
            "expected call_expression in output: {}",
            output.content
        );
        assert!(
            output.content.contains("scoped_identifier"),
            "expected scoped_identifier (Rust grammar) in output: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn snippet_mode_parses_python_attribute_chain() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "json.loads(payload)",
            "language": "python"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("attribute"),
            "expected `attribute` node in output: {}",
            output.content
        );
        assert!(
            output.content.contains("argument_list"),
            "expected argument_list: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn path_mode_parses_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub fn hello() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let tool = AstDescribeTool;
        let input = serde_json::json!({ "path": "lib.rs" });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("function_item"));
        assert!(output.content.contains("macro_invocation"));
    }

    #[tokio::test]
    async fn line_range_narrows_output() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn alpha() {}\n\
             fn beta() {\n    \
                 let x = 1;\n\
             }\n\
             fn gamma() {}\n",
        )
        .unwrap();

        let tool = AstDescribeTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "line_range": "2-4"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        // Must include beta's body
        assert!(
            output.content.contains("let_declaration"),
            "expected let_declaration from line 3: {}",
            output.content
        );
        // Must not include alpha or gamma (outside range)
        assert!(
            !output.content.contains("alpha"),
            "line_range should have excluded alpha: {}",
            output.content
        );
        assert!(
            !output.content.contains("gamma"),
            "line_range should have excluded gamma: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn single_line_range_works() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        )
        .unwrap();

        let tool = AstDescribeTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "line_range": "2"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("beta"));
        assert!(!output.content.contains("alpha"));
        assert!(!output.content.contains("gamma"));
    }

    #[tokio::test]
    async fn unknown_language_errors() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "let x = 1",
            "language": "fortran"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("unknown language"));
    }

    #[tokio::test]
    async fn mutually_exclusive_inputs_errors() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "fn f() {}",
            "language": "rust",
            "path": "x.rs"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not both"));
    }

    #[tokio::test]
    async fn no_input_errors() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("snippet") && output.content.contains("path"));
    }

    #[tokio::test]
    async fn snippet_requires_language() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "fn f() {}"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await;
        assert!(output.is_err(), "expected DysonError for missing language");
    }

    #[tokio::test]
    async fn path_language_inferred_from_extension() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("app.py"), "def hello(): pass\n").unwrap();

        let tool = AstDescribeTool;
        let input = serde_json::json!({ "path": "app.py" });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("function_definition"));
    }

    #[tokio::test]
    async fn invalid_line_range_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn f() {}\n").unwrap();

        let tool = AstDescribeTool;
        let input = serde_json::json!({
            "path": "lib.rs",
            "line_range": "5-2"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("greater than"));
    }

    #[tokio::test]
    async fn max_depth_truncates() {
        let tool = AstDescribeTool;
        let tmp = tempfile::tempdir().unwrap();
        let input = serde_json::json!({
            "snippet": "fn f() { g(h(i(j()))); }",
            "language": "rust",
            "max_depth": 2
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("truncated at max_depth"),
            "expected depth-truncation marker: {}",
            output.content
        );
    }

    #[test]
    fn is_agent_only() {
        assert!(AstDescribeTool.agent_only());
    }

    #[test]
    fn parse_line_range_single() {
        assert_eq!(parse_line_range("42").unwrap(), (42, 42));
    }

    #[test]
    fn parse_line_range_span() {
        assert_eq!(parse_line_range("10-20").unwrap(), (10, 20));
        assert_eq!(parse_line_range(" 10 - 20 ").unwrap(), (10, 20));
    }

    #[test]
    fn parse_line_range_rejects_zero() {
        assert!(parse_line_range("0").is_err());
        assert!(parse_line_range("0-5").is_err());
    }

    #[test]
    fn parse_line_range_rejects_reversed() {
        assert!(parse_line_range("10-5").is_err());
    }
}
