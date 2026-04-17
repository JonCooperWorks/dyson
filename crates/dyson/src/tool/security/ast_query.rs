// ===========================================================================
// AstQueryTool — execute tree-sitter S-expression queries against the
// codebase.
//
// This is the security_engineer agent's core power tool.  The agent writes
// tree-sitter query patterns (S-expressions) and this tool compiles and
// runs them against all matching files.  The agent can trace any structural
// pattern through the AST — SQL injection sinks, command injection vectors,
// hardcoded secrets, unsafe blocks, etc.
//
// Example query (find Python eval calls with non-literal arguments):
//   (call function: (identifier) @fn (#eq? @fn "eval")
//     arguments: (argument_list (_) @arg)) @call
//
// Safety:
//   - Read-only — no file modifications
//   - Per-file timeout via QueryCursor::set_timeout_micros
//   - File count cap (MAX_FILES)
//   - Output byte cap (MAX_OUTPUT_BYTES)
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Query, QueryCursor};

use crate::error::{DysonError, Result};
use crate::ast;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::MAX_OUTPUT_BYTES;

/// Per-file query timeout: 5 seconds.
const QUERY_TIMEOUT_MICROS: u64 = 5_000_000;

/// Maximum matches to collect before stopping.
const MAX_MATCHES: usize = 500;

pub struct AstQueryTool;

#[async_trait]
impl Tool for AstQueryTool {
    fn name(&self) -> &str {
        "ast_query"
    }

    fn description(&self) -> &str {
        "Execute a tree-sitter S-expression query against the codebase.  \
         Write a structural pattern to find AST nodes matching specific shapes — \
         function calls, assignments, control flow, etc.  Supports all 20 languages \
         Dyson has grammars for.  Use capture names (@name) to extract matched nodes.  \
         Supports predicates: #eq?, #match?, #not-eq?, #not-match?.  \
         Returns file:line with captured node text and surrounding code context."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Tree-sitter S-expression query pattern.  Use capture names \
                        (@name) to extract matched nodes.  Example: \
                        '(call_expression function: (identifier) @fn (#eq? @fn \"eval\"))'"
                },
                "language": {
                    "type": "string",
                    "description": "Target language: rust, python, javascript, typescript, tsx, \
                        go, java, c, cpp, csharp, ruby, kotlin, swift, zig, elixir, erlang, \
                        ocaml, haskell, nix, json.  Also accepts aliases: js, ts, py, rb, rs, \
                        c++, c#, hs, kt, ex, erl, ml, golang.  Optional when `include` \
                        uniquely implies a language via its file extension (e.g. '*.rs', \
                        '**/*.py')."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (relative to working directory). \
                        Defaults to working directory."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. '*.py', 'src/**/*.rs')"
                }
            },
            "required": ["query"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let query_str = input["query"]
            .as_str()
            .ok_or_else(|| DysonError::tool("ast_query", "missing or invalid 'query'"))?;

        let include_glob = input["include"].as_str().map(String::from);

        // `language` is optional: if omitted, infer from `include`'s extension.
        // This sidesteps the common LLM mistake of forgetting the field when
        // the extension already pins the language unambiguously.
        let config = match input["language"].as_str() {
            Some(name) => match ast::config_for_language_name(name) {
                Some(c) => c,
                None => {
                    return Ok(ToolOutput::error(format!(
                        "unknown language '{name}'.  Supported: rust, python, javascript, \
                         typescript, tsx, go, java, c, cpp, csharp, ruby, kotlin, swift, zig, \
                         elixir, erlang, ocaml, haskell, nix, json"
                    )));
                }
            },
            None => match include_glob.as_deref().and_then(extract_ext).and_then(ast::config_for_extension) {
                Some(c) => c,
                None => {
                    return Ok(ToolOutput::error(
                        "missing 'language' field and couldn't infer it from `include`. \
                         Pass `language` explicitly (e.g. 'rust', 'python') or use an \
                         `include` pattern ending in a known extension like '*.rs' or '*.py'.",
                    ));
                }
            },
        };

        // Compile the query — return a helpful error on invalid syntax.
        let query = match Query::new(&config.language, query_str) {
            Ok(q) => q,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "invalid tree-sitter query: {e}"
                )));
            }
        };

        let search_dir = if let Some(sub) = input["path"].as_str() {
            match super::super::resolve_and_validate_path(&ctx.working_dir, sub, ctx.dangerous_no_sandbox) {
                Ok(resolved) => resolved,
                Err(e) => return Ok(ToolOutput::error(e)),
            }
        } else {
            ctx.working_dir.clone()
        };

        if !search_dir.exists() {
            return Ok(ToolOutput::error(format!(
                "directory does not exist: '{}'",
                search_dir.display()
            )));
        }

        let working_dir_canon = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        // CPU-bound: compile query + walk AST.
        let results = tokio::task::spawn_blocking(move || {
            run_query(
                &query,
                config,
                &search_dir,
                &working_dir_canon,
                include_glob.as_deref(),
            )
        })
        .await
        .map_err(|e| DysonError::tool("ast_query", format!("query task failed: {e}")))?;

        if results.is_empty() {
            return Ok(ToolOutput::success("No matches found."));
        }

        let mut output = results.join("\n");
        if results.len() >= MAX_MATCHES {
            write!(&mut output, "\n\n... (truncated at {MAX_MATCHES} matches)").unwrap();
        }

        Ok(ToolOutput::success(output))
    }
}

/// Run the compiled query against all matching files in `search_dir`.
fn run_query(
    query: &Query,
    config: &ast::LanguageConfig,
    search_dir: &std::path::Path,
    working_dir_canon: &std::path::Path,
    include_glob: Option<&str>,
) -> Vec<String> {
    let mut results = Vec::new();
    let mut total_bytes = 0usize;
    let mut file_count = 0usize;

    let mut builder = ignore::WalkBuilder::new(search_dir);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_global(true);

    if let Some(glob) = include_glob {
        let mut types_builder = ignore::types::TypesBuilder::new();
        types_builder.add("filter", glob).ok();
        types_builder.select("filter");
        if let Ok(types) = types_builder.build() {
            builder.types(types);
        }
    }

    let capture_names: Vec<&str> = query.capture_names().to_vec();

    for entry in builder.build().flatten() {
        if results.len() >= MAX_MATCHES
            || total_bytes >= MAX_OUTPUT_BYTES
            || file_count >= ast::MAX_FILES
        {
            break;
        }

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        // Only process files whose extension maps to the requested language config.
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let file_config = match ast::config_for_extension(ext) {
            Some(c) => c,
            None => continue,
        };
        // Pointer comparison: ensure this file's language matches the query's target.
        if !std::ptr::eq(file_config, config) {
            continue;
        }

        let parsed = match ast::try_parse_file(path, working_dir_canon, false) {
            Ok(Some((_cfg, pf))) => pf,
            _ => continue,
        };

        file_count += 1;

        let source_bytes = parsed.source.as_bytes();
        let mut cursor = QueryCursor::new();
        cursor.set_timeout_micros(QUERY_TIMEOUT_MICROS);

        // QueryMatches implements StreamingIterator (not Iterator) because
        // the underlying C library reuses the match struct on each advance.
        let mut matches = cursor.matches(query, parsed.tree.root_node(), source_bytes);
        while let Some(m) = matches.next() {
            if results.len() >= MAX_MATCHES || total_bytes >= MAX_OUTPUT_BYTES {
                break;
            }

            for capture in m.captures {
                if results.len() >= MAX_MATCHES || total_bytes >= MAX_OUTPUT_BYTES {
                    break;
                }

                let node = capture.node;
                let capture_name = capture_names
                    .get(capture.index as usize)
                    .unwrap_or(&"?");

                let node_text = &parsed.source
                    [node.start_byte()..node.end_byte().min(parsed.source.len())];
                // Truncate very long node texts to avoid flooding output.
                let display_text: std::borrow::Cow<'_, str> = if node_text.len() > 120 {
                    format!("{}...", &node_text[..120]).into()
                } else {
                    node_text.into()
                };

                let line_num = node.start_position().row + 1;
                let context_line = line_at_row(&parsed.source, node.start_position().row);

                let entry = format!(
                    "{}:{}: @{} = {:?} | {}",
                    parsed.rel_path, line_num, capture_name, display_text, context_line,
                );
                total_bytes += entry.len() + 1;
                results.push(entry);
            }
        }
    }

    results
}

/// Extract the text of a specific row (0-indexed) from source.
fn line_at_row(source: &str, row: usize) -> &str {
    source.split('\n').nth(row).unwrap_or("").trim_end()
}

/// Pull the trailing extension out of an `include` glob like `*.rs`,
/// `src/**/*.py`, or `routes/*.{ts,tsx}`.  Returns `None` if the pattern
/// doesn't end in a literal extension — brace expansions with multiple
/// extensions are rejected so we never guess wrong.
fn extract_ext(pattern: &str) -> Option<&str> {
    let last_dot = pattern.rfind('.')?;
    let ext = &pattern[last_dot + 1..];
    if ext.is_empty() || ext.contains(['{', '}', '*', '?', '/', ',']) {
        return None;
    }
    Some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn finds_rust_function_definitions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn hello() {}\nfn world() {}\nstruct Foo;\n",
        )
        .unwrap();

        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(function_item name: (identifier) @fn_name)",
            "language": "rust"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("hello"), "output: {}", output.content);
        assert!(output.content.contains("world"), "output: {}", output.content);
        // Should NOT match struct Foo.
        assert!(!output.content.contains("Foo"), "output: {}", output.content);
    }

    #[tokio::test]
    async fn finds_python_call_expressions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "import os\nos.system('ls')\nprint('hello')\n",
        )
        .unwrap();

        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(call function: (attribute attribute: (identifier) @method (#eq? @method \"system\")))",
            "language": "python"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("system"), "output: {}", output.content);
        // Should not match print.
        assert!(!output.content.contains("print"), "output: {}", output.content);
    }

    #[tokio::test]
    async fn invalid_query_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(this_is_not_valid @@@",
            "language": "rust"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("invalid tree-sitter query"));
    }

    #[tokio::test]
    async fn unknown_language_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(identifier) @id",
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
    async fn no_matches_returns_message() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "struct Foo;\n").unwrap();

        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(function_item name: (identifier) @fn_name)",
            "language": "rust"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No matches"));
    }

    #[tokio::test]
    async fn language_aliases_work() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.js"), "function foo() {}\n").unwrap();

        let tool = AstQueryTool;
        // Use "js" alias instead of "javascript"
        let input = serde_json::json!({
            "query": "(function_declaration name: (identifier) @fn)",
            "language": "js"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("foo"), "output: {}", output.content);
    }

    #[tokio::test]
    async fn only_matches_target_language() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();
        std::fs::write(tmp.path().join("app.py"), "def hello():\n    pass\n").unwrap();

        let tool = AstQueryTool;
        // Query for Rust function items — should not match the Python file.
        let input = serde_json::json!({
            "query": "(function_item name: (identifier) @fn_name)",
            "language": "rust"
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("lib.rs"), "output: {}", output.content);
        assert!(!output.content.contains("app.py"), "output: {}", output.content);
    }

    #[test]
    fn is_agent_only() {
        assert!(AstQueryTool.agent_only());
    }

    #[test]
    fn extract_ext_from_glob() {
        assert_eq!(extract_ext("*.rs"), Some("rs"));
        assert_eq!(extract_ext("src/**/*.py"), Some("py"));
        assert_eq!(extract_ext("file.tsx"), Some("tsx"));
    }

    #[test]
    fn extract_ext_rejects_ambiguous() {
        // Brace expansion with multiple extensions — we refuse to guess.
        assert_eq!(extract_ext("*.{ts,tsx}"), None);
        assert_eq!(extract_ext("src/**/*"), None);
        assert_eq!(extract_ext("noext"), None);
        assert_eq!(extract_ext("trailing."), None);
    }

    #[tokio::test]
    async fn language_inferred_from_include() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hi.rs"), "fn hi() {}\n").unwrap();

        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(function_item name: (identifier) @fn)",
            "include": "*.rs",
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(output.content.contains("hi"), "output: {}", output.content);
    }

    #[tokio::test]
    async fn missing_language_with_ambiguous_include_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = AstQueryTool;
        let input = serde_json::json!({
            "query": "(function_item name: (identifier) @fn)",
            "include": "*.{ts,tsx}",
        });
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(
            output.content.contains("language"),
            "output: {}",
            output.content
        );
    }
}
