// ===========================================================================
// find_replace — plain text or regex find-and-replace across files.
//
// Path-scoped (file or directory).  Walks directories respecting
// .gitignore, skips binary / non-UTF-8 files silently, honors size and
// file-count limits.
//
// Supports an optional regex mode with capture groups ($1, $2, etc.).
// No AST — use rename_symbol if you want identifier-aware behavior.
// ===========================================================================

use std::path::Path;

use regex::Regex;

use crate::error::Result;
use crate::tool::ToolOutput;

use crate::tool::ast::{self, MAX_FILE_SIZE};

/// Maximum number of files to process in a single find_replace.
///
/// Kept lower than the AST limit because text replaces can touch many more
/// files (any UTF-8 text, not just registered grammars).
const MAX_FILES: usize = 200;

/// Find and replace `pattern` with `replacement` across files under `path`.
pub fn find_replace(
    resolved_path: &Path,
    working_dir: &Path,
    pattern: &str,
    replacement: &str,
    use_regex: bool,
    dry_run: bool,
) -> Result<ToolOutput> {
    let compiled = if use_regex {
        match Regex::new(pattern) {
            Ok(r) => Some(r),
            Err(e) => {
                return Ok(ToolOutput::error(format!("invalid regex pattern: {e}")));
            }
        }
    } else {
        None
    };

    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut edits: Vec<(String, usize)> = Vec::new();
    let mut total = 0usize;
    let mut skipped_large = 0usize;
    let mut skipped_binary = 0usize;

    if resolved_path.is_file() {
        match process_file(
            resolved_path,
            &working_dir_canon,
            pattern,
            replacement,
            compiled.as_ref(),
            dry_run,
        ) {
            FileResult::Modified { rel, count } => {
                total += count;
                edits.push((rel, count));
            }
            FileResult::NoMatch => {}
            FileResult::SkippedLarge => skipped_large += 1,
            FileResult::SkippedBinary => skipped_binary += 1,
            FileResult::WriteFailed { rel } => {
                edits.push((format!("{rel} (write failed)"), 0));
            }
        }
    } else if resolved_path.is_dir() {
        for entry in ast::walk_dir(resolved_path).flatten() {
            if edits.len() >= MAX_FILES {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match process_file(
                path,
                &working_dir_canon,
                pattern,
                replacement,
                compiled.as_ref(),
                dry_run,
            ) {
                FileResult::Modified { rel, count } => {
                    total += count;
                    edits.push((rel, count));
                }
                FileResult::NoMatch => {}
                FileResult::SkippedLarge => skipped_large += 1,
                FileResult::SkippedBinary => skipped_binary += 1,
                FileResult::WriteFailed { rel } => {
                    edits.push((format!("{rel} (write failed)"), 0));
                }
            }
        }
    } else {
        return Ok(ToolOutput::error(format!(
            "path '{}' is not a file or directory",
            resolved_path.display()
        )));
    }

    let json = serde_json::json!({
        "files_modified": edits.len(),
        "occurrences_replaced": total,
        "dry_run": dry_run,
        "skipped_large": skipped_large,
        "skipped_binary": skipped_binary,
        "files": edits.iter().map(|(p, c)| serde_json::json!({
            "path": p,
            "count": c,
        })).collect::<Vec<_>>(),
    });

    Ok(ToolOutput::success(json.to_string()))
}

enum FileResult {
    Modified { rel: String, count: usize },
    NoMatch,
    SkippedLarge,
    SkippedBinary,
    WriteFailed { rel: String },
}

fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    pattern: &str,
    replacement: &str,
    compiled: Option<&Regex>,
    dry_run: bool,
) -> FileResult {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return FileResult::SkippedBinary,
    };
    if metadata.len() > MAX_FILE_SIZE {
        return FileResult::SkippedLarge;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return FileResult::SkippedBinary,
    };

    let (count, new_content) = if let Some(re) = compiled {
        let matches = re.find_iter(&content).count();
        if matches == 0 {
            return FileResult::NoMatch;
        }
        let replaced = re.replace_all(&content, replacement).into_owned();
        (matches, replaced)
    } else {
        let matches = content.matches(pattern).count();
        if matches == 0 {
            return FileResult::NoMatch;
        }
        (matches, content.replace(pattern, replacement))
    };

    let rel = path
        .strip_prefix(working_dir_canon)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    if !dry_run && std::fs::write(path, &new_content).is_err() {
        return FileResult::WriteFailed { rel };
    }

    FileResult::Modified { rel, count }
}

/// Validate user-provided pattern/replacement before running.
pub fn validate(
    pattern: &str,
    replacement: &str,
    use_regex: bool,
) -> std::result::Result<(), String> {
    if pattern.is_empty() {
        return Err("pattern must not be empty".into());
    }
    // For literal mode, identical pattern/replacement is a no-op.
    // For regex mode we skip this check — `\.` → `\.` is unusual but legal.
    if !use_regex && pattern == replacement {
        return Err("pattern and replacement are identical — nothing to do".into());
    }
    if use_regex && let Err(e) = Regex::new(pattern) {
        return Err(format!("invalid regex pattern: {e}"));
    }
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        path: &Path,
        wd: &Path,
        pattern: &str,
        replacement: &str,
        use_regex: bool,
        dry_run: bool,
    ) -> serde_json::Value {
        let output = find_replace(path, wd, pattern, replacement, use_regex, dry_run).unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        serde_json::from_str(&output.content).unwrap()
    }

    #[test]
    fn dry_run_previews_changes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "fn old_name() {}\nfn old_name_call() { old_name(); }\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("b.rs"), "use old_name;\n").unwrap();

        let json = run(tmp.path(), tmp.path(), "old_name", "new_name", false, true);
        assert_eq!(json["dry_run"], true);
        assert!(json["files_modified"].as_u64().unwrap() >= 2);
        assert!(json["occurrences_replaced"].as_u64().unwrap() >= 3);

        // Files must NOT have been modified.
        let a = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(a.contains("old_name"));
        assert!(!a.contains("new_name"));
    }

    #[test]
    fn apply_modifies_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn old_name() {}\n").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "use old_name;\n").unwrap();

        let _json = run(tmp.path(), tmp.path(), "old_name", "new_name", false, false);

        let a = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(a.contains("new_name"));
        assert!(!a.contains("old_name"));

        let b = std::fs::read_to_string(tmp.path().join("b.rs")).unwrap();
        assert!(b.contains("new_name"));
    }

    #[test]
    fn no_matches_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\n").unwrap();

        let json = run(tmp.path(), tmp.path(), "zzzzz", "yyyyy", false, false);
        assert_eq!(json["files_modified"], 0);
        assert_eq!(json["occurrences_replaced"], 0);
    }

    #[test]
    fn validates_empty_pattern() {
        assert!(validate("", "anything", false).is_err());
    }

    #[test]
    fn validates_identical_literal() {
        assert!(validate("same", "same", false).is_err());
        // In regex mode identical is allowed (semantics differ).
        assert!(validate("same", "same", true).is_ok());
    }

    #[test]
    fn skips_binary_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("binary.rs"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn old_name() {}\n").unwrap();

        let json = run(tmp.path(), tmp.path(), "old_name", "new_name", false, false);
        // Only the good file is modified.
        assert_eq!(json["files_modified"], 1);
    }

    #[test]
    fn regex_with_capture_groups() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "fn foo_old() {}\nfn bar_old() {}\nfn baz() {}\n",
        )
        .unwrap();

        let json = run(
            tmp.path(),
            tmp.path(),
            r"(\w+)_old",
            "${1}_new",
            true,
            false,
        );
        assert_eq!(json["files_modified"], 1);
        assert_eq!(json["occurrences_replaced"], 2);

        let a = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(a.contains("fn foo_new()"));
        assert!(a.contains("fn bar_new()"));
        assert!(a.contains("fn baz()"));
        assert!(!a.contains("_old"));
    }

    #[test]
    fn regex_invalid_pattern_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();

        let output = find_replace(tmp.path(), tmp.path(), "(unclosed", "x", true, false).unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("invalid regex"));
    }

    #[test]
    fn url_rewrite_literal() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "const URL: &str = \"http://example.com\";\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.md"),
            "Visit http://example.com for info.\n",
        )
        .unwrap();

        let _json = run(tmp.path(), tmp.path(), "http://", "https://", false, false);

        let a = std::fs::read_to_string(tmp.path().join("a.rs")).unwrap();
        assert!(a.contains("https://example.com"));
        let b = std::fs::read_to_string(tmp.path().join("b.md")).unwrap();
        assert!(b.contains("https://example.com"));
    }
}
