// ===========================================================================
// BulkEdit tool — multi-file find-and-replace with glob patterns.
//
// Uses the `ignore` crate (same as search_files) to walk directories
// respecting .gitignore, and applies string replacements across all
// matching files in a single tool call.
//
// Safety: defaults to dry_run=true so the LLM must explicitly opt in
// to writing.  File size and file count limits prevent runaway edits.
// ===========================================================================

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

/// Maximum number of files to process in a single bulk edit.
const MAX_FILES: usize = 200;

/// Maximum file size to process (10 MB, same as edit_file).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

pub struct BulkEditTool;

#[async_trait]
impl Tool for BulkEditTool {
    fn name(&self) -> &str {
        "bulk_edit"
    }

    fn description(&self) -> &str {
        "Find and replace a string across multiple files matching a glob pattern. \
         Defaults to dry_run=true to preview changes safely. Set dry_run=false to \
         apply. Respects .gitignore."
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
                    "description": "Glob pattern for files to edit (e.g. '*.rs', 'src/**/*.py')"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace in each file"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Preview changes without applying (default: true)"
                }
            },
            "required": ["pattern", "old_string", "new_string"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = input["pattern"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bulk_edit", "missing or invalid 'pattern'"))?;
        let old_string = input["old_string"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bulk_edit", "missing or invalid 'old_string'"))?;
        let new_string = input["new_string"]
            .as_str()
            .ok_or_else(|| DysonError::tool("bulk_edit", "missing or invalid 'new_string'"))?;
        let dry_run = input["dry_run"].as_bool().unwrap_or(true);

        if old_string.is_empty() {
            return Ok(ToolOutput::error("old_string must not be empty"));
        }

        if old_string == new_string {
            return Ok(ToolOutput::error(
                "old_string and new_string are identical — nothing to do",
            ));
        }

        let working_dir = ctx.working_dir.clone();
        let old = old_string.to_string();
        let new = new_string.to_string();
        let glob = pattern.to_string();

        let results = tokio::task::spawn_blocking(move || {
            do_bulk_edit(&working_dir, &glob, &old, &new, dry_run)
        })
        .await
        .map_err(|e| DysonError::tool("bulk_edit", format!("task failed: {e}")))?;

        let (edits, skipped_large, skipped_binary) = results;

        if edits.is_empty() {
            return Ok(ToolOutput::success(format!(
                "No files matching '{pattern}' contain '{old_string}'."
            )));
        }

        let mode = if dry_run { "DRY RUN" } else { "APPLIED" };
        let files_count = edits.len();
        let mut output = format!("## {mode}: {files_count} file(s) affected\n\n");

        let mut total_replacements = 0usize;
        for (path, count) in &edits {
            output.push_str(&format!("- {path}: {count} replacement(s)\n"));
            total_replacements += count;
        }

        output.push_str(&format!(
            "\nTotal: {total_replacements} replacement(s) across {files_count} file(s)"
        ));

        if skipped_large > 0 {
            output.push_str(&format!("\nSkipped {skipped_large} file(s) over 10 MB"));
        }
        if skipped_binary > 0 {
            output.push_str(&format!(
                "\nSkipped {skipped_binary} binary/unreadable file(s)"
            ));
        }

        if dry_run {
            output.push_str("\n\nRe-run with dry_run=false to apply changes.");
        }

        Ok(ToolOutput::success(output))
    }
}

/// Perform the bulk edit on a blocking thread.
///
/// Returns (edits, skipped_large, skipped_binary) where edits is a vec
/// of (relative_path, replacement_count) for each modified file.
fn do_bulk_edit(
    working_dir: &std::path::Path,
    glob: &str,
    old: &str,
    new: &str,
    dry_run: bool,
) -> (Vec<(String, usize)>, usize, usize) {
    let mut builder = ignore::WalkBuilder::new(working_dir);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_global(true);

    // Use overrides for full glob pattern support (paths + extensions).
    let mut overrides = ignore::overrides::OverrideBuilder::new(working_dir);
    // ignore crate treats override patterns as gitignore-style: a leading
    // `!` negates.  We add the user's glob as a positive match.
    if overrides.add(glob).is_err() {
        return (Vec::new(), 0, 0);
    }
    if let Ok(built) = overrides.build() {
        builder.overrides(built);
    }

    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut edits: Vec<(String, usize)> = Vec::new();
    let mut skipped_large = 0usize;
    let mut skipped_binary = 0usize;

    for entry in builder.build().flatten() {
        if edits.len() >= MAX_FILES {
            break;
        }

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.len() > MAX_FILE_SIZE {
            skipped_large += 1;
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                skipped_binary += 1;
                continue;
            }
        };

        let count = content.matches(old).count();
        if count == 0 {
            continue;
        }

        let rel_path = path
            .strip_prefix(&working_dir_canon)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());

        if !dry_run {
            let new_content = content.replace(old, new);
            if let Err(_e) = std::fs::write(path, &new_content) {
                // Still record the file but with 0 to signal an error.
                edits.push((format!("{rel_path} (write failed)"), 0));
                continue;
            }
        }

        edits.push((rel_path, count));
    }

    (edits, skipped_large, skipped_binary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn dry_run_previews_changes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn old_name() {}\nfn old_name_call() { old_name(); }\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "use old_name;\n").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "no match\n").unwrap();

        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "old_name",
            "new_string": "new_name",
            "dry_run": true
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("DRY RUN"));
        assert!(output.content.contains("a.rs"));
        assert!(output.content.contains("b.rs"));
        assert!(!output.content.contains("c.txt"));

        // Verify files were NOT modified.
        let content = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(content.contains("old_name"));
    }

    #[tokio::test]
    async fn apply_modifies_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn old_name() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "use old_name;\n").unwrap();

        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "old_name",
            "new_string": "new_name",
            "dry_run": false
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("APPLIED"));

        let a = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(a.contains("new_name"));
        assert!(!a.contains("old_name"));

        let b = std::fs::read_to_string(tmp.path().join("b.rs")).unwrap();
        assert!(b.contains("new_name"));
    }

    #[tokio::test]
    async fn no_matches_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\n").unwrap();

        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "zzzzz",
            "new_string": "yyyyy"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No files"));
    }

    #[tokio::test]
    async fn rejects_empty_old_string() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "",
            "new_string": "something"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn rejects_identical_strings() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "same",
            "new_string": "same"
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("identical"));
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a binary file with a .rs extension.
        std::fs::write(tmp.path().join("binary.rs"), &[0u8, 159, 146, 150]).unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn old_name() {}\n").unwrap();

        let tool = BulkEditTool;
        let input = serde_json::json!({
            "pattern": "*.rs",
            "old_string": "old_name",
            "new_string": "new_name",
            "dry_run": false
        });
        let output = tool.run(&input, &ToolContext::for_test(tmp.path())).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("good.rs"));
    }

    #[test]
    fn is_agent_only() {
        assert!(BulkEditTool.agent_only());
    }
}
