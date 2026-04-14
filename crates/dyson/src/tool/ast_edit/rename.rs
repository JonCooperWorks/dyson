// ===========================================================================
// rename_symbol — AST-aware identifier renaming.
//
// Walks tree-sitter ASTs to find identifier nodes matching `old_name`,
// then replaces them with `new_name`.  Strings, comments, and other
// non-identifier text are never touched.
//
// Supports single files and recursive directory walks (with .gitignore).
// ===========================================================================

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::error::Result;
use crate::tool::ToolOutput;

use super::languages;

/// Maximum file size for AST parsing (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum number of files to process in a single rename.
const MAX_FILES: usize = 500;

/// Rename all occurrences of `old_name` to `new_name` in the given path.
///
/// `resolved_path` may be a single file or a directory (recursive walk).
/// Returns a JSON summary as a ToolOutput.
pub fn rename_symbol(
    resolved_path: &Path,
    working_dir: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<ToolOutput> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut files_result: Vec<FileRenameResult> = Vec::new();
    let mut total_occurrences = 0usize;

    if resolved_path.is_file() {
        if let Some(result) = process_file(resolved_path, &working_dir_canon, old_name, new_name)? {
            total_occurrences += result.occurrences;
            files_result.push(result);
        }
    } else if resolved_path.is_dir() {
        let mut builder = ignore::WalkBuilder::new(resolved_path);
        builder.hidden(false);
        builder.git_ignore(true);
        builder.git_global(true);

        for entry in builder.build().flatten() {
            if files_result.len() >= MAX_FILES {
                break;
            }

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if let Some(result) = process_file(path, &working_dir_canon, old_name, new_name)? {
                total_occurrences += result.occurrences;
                files_result.push(result);
            }
        }
    } else {
        return Ok(ToolOutput::error(format!(
            "path '{}' is not a file or directory",
            resolved_path.display()
        )));
    }

    let json = serde_json::json!({
        "files_modified": files_result.len(),
        "occurrences_renamed": total_occurrences,
        "files": files_result.iter().map(|r| serde_json::json!({
            "path": r.path,
            "occurrences": r.occurrences,
        })).collect::<Vec<_>>(),
    });

    Ok(ToolOutput::success(json.to_string()))
}

struct FileRenameResult {
    path: String,
    occurrences: usize,
}

/// Process a single file for renaming.  Returns `None` if the file was
/// skipped (wrong extension, binary, too large, no matches).
fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<Option<FileRenameResult>> {
    // Get language config from extension.
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return Ok(None),
    };
    let config = match languages::config_for_extension(ext) {
        Some(c) => c,
        None => return Ok(None),
    };

    // JSON doesn't support rename.
    if config.identifier_types.is_empty() {
        return Ok(None);
    }

    // Check file size.
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if metadata.len() > MAX_FILE_SIZE {
        return Ok(None);
    }

    // Read file (skip binary/unreadable).
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    // Parse with tree-sitter.
    let mut parser = Parser::new();
    parser
        .set_language(&config.language)
        .map_err(|e| crate::error::DysonError::tool("ast_edit", format!("parser setup: {e}")))?;

    let tree = match parser.parse(&source, None) {
        Some(t) => t,
        None => return Ok(None),
    };

    // Collect matching identifier positions.
    let mut matches: Vec<(usize, usize)> = Vec::new();
    collect_matching_identifiers(
        tree.root_node(),
        source.as_bytes(),
        old_name,
        config.identifier_types,
        &mut matches,
    );

    if matches.is_empty() {
        return Ok(None);
    }

    // Sort by start_byte descending — replace from end to start so
    // earlier byte offsets remain valid.
    matches.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = source;
    for (start, end) in &matches {
        result.replace_range(*start..*end, new_name);
    }

    std::fs::write(path, &result)
        .map_err(|e| crate::error::DysonError::tool("ast_edit", format!("write failed: {e}")))?;

    let rel_path = path
        .strip_prefix(working_dir_canon)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    Ok(Some(FileRenameResult {
        path: rel_path,
        occurrences: matches.len(),
    }))
}

/// Recursively collect all identifier nodes matching `target_name`.
fn collect_matching_identifiers(
    node: Node<'_>,
    source: &[u8],
    target_name: &str,
    id_types: &[&str],
    results: &mut Vec<(usize, usize)>,
) {
    if id_types.contains(&node.kind())
        && let Ok(text) = std::str::from_utf8(&source[node.start_byte()..node.end_byte()])
        && text == target_name
    {
        results.push((node.start_byte(), node.end_byte()));
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_matching_identifiers(child, source, target_name, id_types, results);
    }
}
