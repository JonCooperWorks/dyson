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

use tree_sitter::Node;

use crate::error::Result;
use crate::tool::ToolOutput;

use super::languages::{self, MAX_FILES};

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

    let mut files: Vec<(String, usize)> = Vec::new();
    let mut total = 0usize;

    if resolved_path.is_file() {
        if let Some((path, count)) =
            process_file(resolved_path, &working_dir_canon, old_name, new_name)?
        {
            total += count;
            files.push((path, count));
        }
    } else if resolved_path.is_dir() {
        for entry in languages::walk_dir(resolved_path).flatten() {
            if files.len() >= MAX_FILES {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some((rel, count)) =
                process_file(path, &working_dir_canon, old_name, new_name)?
            {
                total += count;
                files.push((rel, count));
            }
        }
    } else {
        return Ok(ToolOutput::error(format!(
            "path '{}' is not a file or directory",
            resolved_path.display()
        )));
    }

    let json = serde_json::json!({
        "files_modified": files.len(),
        "occurrences_renamed": total,
        "files": files.iter().map(|(p, c)| serde_json::json!({
            "path": p,
            "occurrences": c,
        })).collect::<Vec<_>>(),
    });

    Ok(ToolOutput::success(json.to_string()))
}

/// Process a single file for renaming.  Returns `None` if skipped.
fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<Option<(String, usize)>> {
    let (config, parsed) =
        match languages::try_parse_file(path, working_dir_canon, true)? {
            Some(pair) => pair,
            None => return Ok(None),
        };

    let mut matches: Vec<(usize, usize)> = Vec::new();
    collect_matching_identifiers(
        parsed.tree.root_node(),
        parsed.source.as_bytes(),
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

    let mut result = parsed.source;
    for (start, end) in &matches {
        result.replace_range(*start..*end, new_name);
    }

    std::fs::write(path, &result)
        .map_err(|e| crate::error::DysonError::tool("ast_edit", format!("write failed: {e}")))?;

    Ok(Some((parsed.rel_path, matches.len())))
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
