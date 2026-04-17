// ===========================================================================
// SearchFiles tool — regex or AST-aware content search across files.
//
// Default mode: `pattern` is a regex, matched line-by-line with grep
// semantics.  Output: `path:line: text`.
//
// AST mode (`ast: true`): `pattern` is treated as a literal identifier
// name (not a regex).  For files with a tree-sitter grammar, only
// identifier nodes matching that name count — strings, comments, and
// substrings of longer identifiers (`Config` inside `ConfigManager`) are
// ignored.  For files without a grammar (Markdown, YAML, configs), the
// search falls back to a word-boundary literal match with the same safety
// guarantee.  Use AST mode to audit symbol usage before/after a rename;
// use regex mode for everything else.
// ===========================================================================

use std::fmt::Write;
use std::io::{BufRead, BufReader};
use std::path::Path;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::ast;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::MAX_OUTPUT_BYTES;

/// Maximum number of matching lines to collect.
const MAX_MATCHES: usize = 500;

pub struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Search file contents. Two modes: \
         regex (default) — `pattern` is a regex, matched line-by-line like grep, returns \
         `path:line: text`. \
         AST (`ast: true`) — `pattern` is a literal identifier name; matches only identifier \
         AST nodes for files with a tree-sitter grammar (strings/comments/substrings ignored), \
         and falls back to word-boundary literal match for files without a grammar (Markdown, \
         YAML, configs). AST mode is symbol-aware — use it to audit where a name is used \
         before/after a rename. Both modes respect .gitignore. \
         Use `include` to filter by file glob (e.g. '*.rs')."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (relative to working directory). Defaults to working directory."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter which files to search (e.g. '*.rs', '*.py')"
                },
                "ast": {
                    "type": "boolean",
                    "description": "If true, treat `pattern` as a literal identifier name and match only AST identifier nodes (with word-boundary text fallback for files without a grammar). Default false (regex mode)."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern_str = input["pattern"]
            .as_str()
            .ok_or_else(|| DysonError::tool("search_files", "missing or invalid 'pattern'"))?;
        let ast_mode = input["ast"].as_bool().unwrap_or(false);

        // AST mode treats `pattern` as a literal identifier name and demands
        // a non-empty string; regex mode validates as regex.
        let regex = if ast_mode {
            if pattern_str.is_empty() {
                return Ok(ToolOutput::error(
                    "ast mode: pattern must not be empty (it is treated as an identifier name)",
                ));
            }
            None
        } else {
            match regex::Regex::new(pattern_str) {
                Ok(r) => Some(r),
                Err(e) => return Ok(ToolOutput::error(format!("invalid regex: {e}"))),
            }
        };

        let search_dir = if let Some(sub) = input["path"].as_str() {
            // Validate the path doesn't escape the working directory.
            match super::resolve_and_validate_path(&ctx.working_dir, sub, ctx.dangerous_no_sandbox) {
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

        let include_glob = input["include"].as_str();

        // Build the directory walker using the `ignore` crate,
        // which respects .gitignore automatically.
        let mut builder = ignore::WalkBuilder::new(&search_dir);
        builder.hidden(false); // search hidden files too
        builder.git_ignore(true);
        builder.git_global(true);

        if let Some(glob) = include_glob {
            // Add a file type glob filter.
            let mut types_builder = ignore::types::TypesBuilder::new();
            types_builder.add("filter", glob).ok();
            types_builder.select("filter");
            if let Ok(types) = types_builder.build() {
                builder.types(types);
            }
        }

        let working_dir_canon = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        let pattern_owned = pattern_str.to_string();
        // Walk and search — this is CPU-bound, so run in a blocking task.
        let results = tokio::task::spawn_blocking(move || {
            let mut matches = Vec::new();
            let mut total_bytes = 0usize;

            for entry in builder.build().flatten() {
                if matches.len() >= MAX_MATCHES || total_bytes >= MAX_OUTPUT_BYTES {
                    break;
                }

                let path = entry.path();
                if !path.is_file() {
                    continue;
                }

                let rel_path = path
                    .strip_prefix(&working_dir_canon)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| path.to_string_lossy().into_owned());

                if ast_mode {
                    scan_file_ast(
                        path,
                        &working_dir_canon,
                        &rel_path,
                        &pattern_owned,
                        &mut matches,
                        &mut total_bytes,
                    );
                } else if let Some(rx) = &regex {
                    scan_file_regex(path, &rel_path, rx, &mut matches, &mut total_bytes);
                }
            }

            matches
        })
        .await
        .map_err(|e| DysonError::tool("search_files", format!("search task failed: {e}")))?;

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

/// Regex mode: scan a file line-by-line and push every line that matches.
fn scan_file_regex(
    path: &Path,
    rel_path: &str,
    regex: &regex::Regex,
    matches: &mut Vec<String>,
    total_bytes: &mut usize,
) {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = BufReader::new(file);
    for (line_num, line_result) in reader.lines().enumerate() {
        if matches.len() >= MAX_MATCHES || *total_bytes >= MAX_OUTPUT_BYTES {
            break;
        }
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break, // binary or non-UTF-8
        };
        if regex.is_match(&line) {
            let entry = format!("{}:{}: {}", rel_path, line_num + 1, line);
            *total_bytes += entry.len() + 1;
            matches.push(entry);
        }
    }
}

/// AST mode: parse the file with tree-sitter (when supported) and report
/// every identifier-node match.  Falls back to word-boundary literal match
/// for files without a registered grammar so docs and configs aren't
/// silently skipped.
fn scan_file_ast(
    path: &Path,
    working_dir_canon: &Path,
    rel_path: &str,
    name: &str,
    matches: &mut Vec<String>,
    total_bytes: &mut usize,
) {
    let ext = path.extension().and_then(|e| e.to_str());
    let ast_capable = ext
        .and_then(ast::config_for_extension)
        .is_some_and(|c| !c.identifier_types.is_empty());

    if ast_capable {
        scan_ast_path(path, working_dir_canon, rel_path, name, matches, total_bytes);
    } else {
        scan_text_fallback(path, rel_path, name, matches, total_bytes);
    }
}

fn scan_ast_path(
    path: &Path,
    working_dir_canon: &Path,
    rel_path: &str,
    name: &str,
    matches: &mut Vec<String>,
    total_bytes: &mut usize,
) {
    let parsed = match ast::try_parse_file(path, working_dir_canon, true) {
        Ok(Some(pair)) => pair,
        _ => return, // skip, oversized, parse failure, or unsupported
    };
    let (config, parsed_file) = parsed;
    let positions = ast::find_identifier_positions(
        &parsed_file.tree,
        parsed_file.source.as_bytes(),
        name,
        config.identifier_types,
    );
    push_positions(&parsed_file.source, rel_path, &positions, matches, total_bytes);
}

fn scan_text_fallback(
    path: &Path,
    rel_path: &str,
    name: &str,
    matches: &mut Vec<String>,
    total_bytes: &mut usize,
) {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return,
    };
    if metadata.len() > ast::MAX_FILE_SIZE {
        return;
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return, // binary / non-UTF-8
    };
    let positions = ast::find_word_boundary_matches(&source, name);
    push_positions(&source, rel_path, &positions, matches, total_bytes);
}

/// Common rendering: turn `(start_byte, _end)` positions into
/// `path:line: line_text` entries, respecting the global match/byte caps.
fn push_positions(
    source: &str,
    rel_path: &str,
    positions: &[(usize, usize)],
    matches: &mut Vec<String>,
    total_bytes: &mut usize,
) {
    for (start, _end) in positions {
        if matches.len() >= MAX_MATCHES || *total_bytes >= MAX_OUTPUT_BYTES {
            break;
        }
        let (line_num, line_text) = line_at_byte(source, *start);
        let entry = format!("{rel_path}:{line_num}: {line_text}");
        *total_bytes += entry.len() + 1;
        matches.push(entry);
    }
}

/// Return (1-indexed line number, line text) for the line containing `byte`.
fn line_at_byte(source: &str, byte: usize) -> (usize, &str) {
    let bytes = source.as_bytes();
    let clamped = byte.min(bytes.len());
    let line_start = bytes[..clamped]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |p| p + 1);
    let line_end = bytes[clamped..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |p| clamped + p);
    let line_num = bytes[..line_start].iter().filter(|&&b| b == b'\n').count() + 1;
    (line_num, &source[line_start..line_end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn search_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "no match here\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "fn \\w+"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("fn hello"));
        assert!(output.content.contains("fn world"));
    }

    #[tokio::test]
    async fn search_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello world\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "zzzzz"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No matches"));
    }

    #[tokio::test]
    async fn search_invalid_regex() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "[invalid"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
    }

    #[test]
    fn is_agent_only() {
        assert!(SearchFilesTool.agent_only());
    }

    #[tokio::test]
    async fn search_respects_include_glob() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn target() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "fn target() {}\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({
            "pattern": "target",
            "include": "*.rs"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("a.rs"));
        // b.txt should be filtered out by the include glob.
        assert!(!output.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn search_truncates_at_max_matches() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a file with more than MAX_MATCHES lines that all match.
        let mut content = String::with_capacity(600 * 16);
        for i in 0..600 {
            writeln!(&mut content, "match_line_{i}").unwrap();
        }
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "match_line"});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("truncated"));
    }

    // -----------------------------------------------------------------------
    // AST mode
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ast_mode_matches_only_identifiers() {
        let tmp = tempfile::tempdir().unwrap();
        // `target` appears as identifier (counts), in a comment (ignored),
        // in a string (ignored), and as a substring of `target_value` (ignored).
        std::fs::write(
            tmp.path().join("a.rs"),
            "fn target() {}\n\
             fn caller() { target(); }\n\
             // target in a comment\n\
             let s = \"target\";\n\
             let target_value = 1;\n",
        )
        .unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "target", "ast": true});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        // Two identifier hits: definition and call site.
        let lines: Vec<&str> = output.content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 hits, got: {}", output.content);
        assert!(lines.iter().any(|l| l.contains("a.rs:1")));
        assert!(lines.iter().any(|l| l.contains("a.rs:2")));
    }

    #[tokio::test]
    async fn ast_mode_text_fallback_for_non_grammar_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Markdown has no grammar — falls back to word-boundary literal.
        std::fs::write(
            tmp.path().join("README.md"),
            "Use Config here.\nConfigManager is different.\n",
        )
        .unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "Config", "ast": true});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        // Line 1 matches `Config`; line 2's `ConfigManager` does not (word boundary).
        assert!(output.content.contains("README.md:1"));
        assert!(!output.content.contains("README.md:2"));
    }

    #[tokio::test]
    async fn ast_mode_finds_method_inside_impl() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "struct Foo;\nimpl Foo {\n    fn target(&self) {}\n    fn other() { Self::target; }\n}\n",
        )
        .unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "target", "ast": true});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        // Both the method definition and the Self::target reference.
        assert!(output.content.contains("a.rs:3"));
        assert!(output.content.contains("a.rs:4"));
    }

    #[tokio::test]
    async fn ast_mode_empty_pattern_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "", "ast": true});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn ast_mode_does_not_treat_pattern_as_regex() {
        // `target.*` would be a valid regex, but in AST mode it's a literal
        // identifier name — there's no identifier with literal dots/stars,
        // so this should match nothing.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn target() {}\n").unwrap();

        let tool = SearchFilesTool;
        let input = serde_json::json!({"pattern": "target.*", "ast": true});
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No matches"));
    }
}
