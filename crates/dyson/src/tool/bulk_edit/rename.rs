// ===========================================================================
// rename_symbol — AST-aware identifier renaming with text fallback.
//
// For files with a registered tree-sitter grammar, walks the AST and
// renames only identifier nodes matching `old_name` exactly (strings,
// comments, and raw-string contents are never touched).
//
// For files without a grammar (e.g. .md, .yaml, .toml, .html), falls back
// to a word-boundary text replace: the char before and after the match
// must be a non-identifier char (or start/end of string) — this prevents
// renaming `Config` inside `ConfigManager`.
//
// Each per-file result reports `method: "ast"` or `method: "text"` so the
// agent can see what happened.
// ===========================================================================

use std::path::Path;

use crate::ast::{
    self, MAX_FILE_SIZE, MAX_FILES, find_identifier_positions, find_word_boundary_matches,
};
use crate::error::Result;
use crate::tool::ToolOutput;

/// Method used to rename a given file.
#[derive(Clone, Copy)]
enum Method {
    Ast,
    Text,
}

impl Method {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Ast => "ast",
            Self::Text => "text",
        }
    }
}

/// Rename all occurrences of `old_name` to `new_name` in the given path.
///
/// `resolved_path` may be a single file or a directory (recursive walk).
/// Returns a JSON summary as a ToolOutput.
pub fn rename_symbol(
    resolved_path: &Path,
    working_dir: &Path,
    old_name: &str,
    new_name: &str,
    dry_run: bool,
) -> Result<ToolOutput> {
    let working_dir_canon = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    let mut files: Vec<(String, usize, Method)> = Vec::new();
    let mut total = 0usize;

    if resolved_path.is_file() {
        if let Some((path, count, method)) = process_file(
            resolved_path,
            &working_dir_canon,
            old_name,
            new_name,
            dry_run,
        )? {
            total += count;
            files.push((path, count, method));
        }
    } else if resolved_path.is_dir() {
        for entry in ast::walk_dir(resolved_path).flatten() {
            if files.len() >= MAX_FILES {
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some((rel, count, method)) =
                process_file(path, &working_dir_canon, old_name, new_name, dry_run)?
            {
                total += count;
                files.push((rel, count, method));
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
        "dry_run": dry_run,
        "files": files.iter().map(|(p, c, m)| serde_json::json!({
            "path": p,
            "count": c,
            "method": m.as_str(),
        })).collect::<Vec<_>>(),
    });

    Ok(ToolOutput::success(json.to_string()))
}

/// Process a single file for renaming.
///
/// Tries AST path first for files with a registered grammar that has
/// identifier types. Falls back to word-boundary text replacement for
/// everything else.
///
/// Returns `None` if the file was skipped or no matches were found.
fn process_file(
    path: &Path,
    working_dir_canon: &Path,
    old_name: &str,
    new_name: &str,
    dry_run: bool,
) -> Result<Option<(String, usize, Method)>> {
    let ext = path.extension().and_then(|e| e.to_str());
    let ast_capable = ext
        .and_then(ast::config_for_extension)
        .is_some_and(|c| !c.identifier_types.is_empty());

    if ast_capable {
        if let Some((rel, count)) =
            process_file_ast(path, working_dir_canon, old_name, new_name, dry_run)?
        {
            return Ok(Some((rel, count, Method::Ast)));
        }
        return Ok(None);
    }

    // Text fallback for unknown extensions and registered-but-identifier-less
    // grammars (e.g. JSON).
    if let Some((rel, count)) =
        process_file_text(path, working_dir_canon, old_name, new_name, dry_run)?
    {
        return Ok(Some((rel, count, Method::Text)));
    }
    Ok(None)
}

/// AST path: parse the file, rename only identifier nodes matching `old_name`.
fn process_file_ast(
    path: &Path,
    working_dir_canon: &Path,
    old_name: &str,
    new_name: &str,
    dry_run: bool,
) -> Result<Option<(String, usize)>> {
    let (config, parsed) = match ast::try_parse_file(path, working_dir_canon, true)? {
        Some(pair) => pair,
        None => return Ok(None),
    };

    let mut matches = find_identifier_positions(
        &parsed.tree,
        parsed.source.as_bytes(),
        old_name,
        config.identifier_types,
    );

    if matches.is_empty() {
        return Ok(None);
    }

    if !dry_run {
        // Sort by start_byte descending — replace from end to start so
        // earlier byte offsets remain valid.
        matches.sort_by(|a, b| b.0.cmp(&a.0));

        let mut result = parsed.source;
        for (start, end) in &matches {
            result.replace_range(*start..*end, new_name);
        }

        std::fs::write(path, &result).map_err(|e| {
            crate::error::DysonError::tool("bulk_edit", format!("write failed: {e}"))
        })?;
    }

    Ok(Some((parsed.rel_path, matches.len())))
}

/// Text fallback: word-boundary match, skip binaries/non-UTF8 and oversized files.
fn process_file_text(
    path: &Path,
    working_dir_canon: &Path,
    old_name: &str,
    new_name: &str,
    dry_run: bool,
) -> Result<Option<(String, usize)>> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if metadata.len() > MAX_FILE_SIZE {
        return Ok(None);
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(None), // binary / non-UTF-8
    };

    let matches = find_word_boundary_matches(&source, old_name);
    if matches.is_empty() {
        return Ok(None);
    }

    let rel_path = path
        .strip_prefix(working_dir_canon)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());

    if !dry_run {
        // Replace back-to-front so earlier offsets stay valid.
        let mut result = source;
        for (start, end) in matches.iter().rev() {
            result.replace_range(*start..*end, new_name);
        }
        std::fs::write(path, &result).map_err(|e| {
            crate::error::DysonError::tool("bulk_edit", format!("write failed: {e}"))
        })?;
    }

    Ok(Some((rel_path, matches.len())))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &Path, wd: &Path, old: &str, new: &str, dry_run: bool) -> serde_json::Value {
        let output = rename_symbol(path, wd, old, new, dry_run).unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        serde_json::from_str(&output.content).unwrap()
    }

    #[test]
    fn rename_rust_basics_ast() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "fn target() -> i32 { 42 }\n\n\
             fn main() {\n    let x = target();\n}\n\
             // target is important\n\
             let s = \"target\";\n",
        )
        .unwrap();

        let json = run(
            &tmp.path().join("lib.rs"),
            tmp.path(),
            "target",
            "renamed",
            false,
        );
        assert_eq!(json["occurrences_renamed"], 2);
        assert_eq!(json["files_modified"], 1);
        assert_eq!(json["files"][0]["method"], "ast");

        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("fn renamed()"));
        assert!(content.contains("renamed();\n"));
        // Comments and strings should NOT be renamed.
        assert!(content.contains("// target is important"));
        assert!(content.contains("\"target\""));
    }

    #[test]
    fn rename_across_directory_ast() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "struct Config { val: i32 }\n").unwrap();
        std::fs::write(
            tmp.path().join("src/b.rs"),
            "fn new_config() -> Config { Config { val: 1 } }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/c.rs"),
            "use crate::Config;\nfn get() -> Config { todo!() }\n",
        )
        .unwrap();

        let json = run(
            &tmp.path().join("src"),
            tmp.path(),
            "Config",
            "AppConfig",
            false,
        );
        assert_eq!(json["files_modified"], 3);
        assert!(json["occurrences_renamed"].as_u64().unwrap() >= 5);

        for name in &["src/a.rs", "src/b.rs", "src/c.rs"] {
            let content = std::fs::read_to_string(tmp.path().join(name)).unwrap();
            assert!(
                content.contains("AppConfig"),
                "{name} should contain AppConfig"
            );
        }
    }

    #[test]
    fn rename_no_matches_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn hello() {}\n").unwrap();

        let json = run(
            &tmp.path().join("lib.rs"),
            tmp.path(),
            "nonexistent",
            "something",
            false,
        );
        assert_eq!(json["files_modified"], 0);
        assert_eq!(json["occurrences_renamed"], 0);
    }

    #[test]
    fn rename_substring_no_match_ast() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "struct Config {}\nstruct ConfigManager {}\n",
        )
        .unwrap();

        run(
            &tmp.path().join("lib.rs"),
            tmp.path(),
            "Config",
            "AppConfig",
            false,
        );

        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("AppConfig"));
        assert!(content.contains("ConfigManager"));
        // ConfigManager should NOT become AppConfigManager.
        assert!(!content.contains("AppConfigManager"));
    }

    #[test]
    fn rename_binary_file_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // Binary file with .rs extension — AST parse rejects invalid UTF-8.
        std::fs::write(tmp.path().join("binary.rs"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn target() {}\n").unwrap();

        let json = run(tmp.path(), tmp.path(), "target", "renamed", false);
        assert_eq!(json["files_modified"], 1);
    }

    #[test]
    fn rename_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

        let json = run(&tmp.path().join("empty"), tmp.path(), "foo", "bar", false);
        assert_eq!(json["files_modified"], 0);
        assert_eq!(json["occurrences_renamed"], 0);
    }

    #[test]
    fn rename_text_fallback_yaml_word_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("deploy.yaml"),
            "Config: production\n\
             ConfigManager: big\n\
             - Config\n\
             description: \"use Config here\"\n",
        )
        .unwrap();

        let json = run(
            &tmp.path().join("deploy.yaml"),
            tmp.path(),
            "Config",
            "AppConfig",
            false,
        );
        assert!(json["occurrences_renamed"].as_u64().unwrap() >= 3);
        assert_eq!(json["files_modified"], 1);
        assert_eq!(json["files"][0]["method"], "text");

        let content = std::fs::read_to_string(tmp.path().join("deploy.yaml")).unwrap();
        // Standalone `Config` becomes `AppConfig`.
        assert!(content.contains("AppConfig: production"));
        // `ConfigManager` stays intact — word boundary prevents substring match.
        assert!(content.contains("ConfigManager: big"));
        assert!(!content.contains("AppConfigManager"));
    }

    #[test]
    fn rename_mixed_ast_and_text() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "struct Config {}\nfn use_config() -> Config { Config {} }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("README.md"),
            "# Config\n\nUse the Config struct.\n",
        )
        .unwrap();

        let json = run(tmp.path(), tmp.path(), "Config", "AppConfig", false);
        assert_eq!(json["files_modified"], 2);

        let files = json["files"].as_array().unwrap();
        let rs = files
            .iter()
            .find(|f| f["path"].as_str().unwrap().ends_with("lib.rs"))
            .unwrap();
        assert_eq!(rs["method"], "ast");

        let md = files
            .iter()
            .find(|f| f["path"].as_str().unwrap().ends_with("README.md"))
            .unwrap();
        assert_eq!(md["method"], "text");

        let md_content = std::fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(md_content.contains("# AppConfig"));
        assert!(md_content.contains("AppConfig struct"));
    }

    #[test]
    fn rename_dry_run_does_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn target() {}\n").unwrap();

        let json = run(
            &tmp.path().join("lib.rs"),
            tmp.path(),
            "target",
            "renamed",
            true,
        );
        assert_eq!(json["files_modified"], 1);
        assert_eq!(json["occurrences_renamed"], 1);
        assert_eq!(json["dry_run"], true);

        // File must NOT have been modified.
        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(content.contains("fn target()"));
        assert!(!content.contains("fn renamed()"));
    }

    #[test]
    fn rename_unsupported_extension_uses_text_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("data.csv"), "target,value\n1,2\n").unwrap();
        std::fs::write(tmp.path().join("good.rs"), "fn target() {}\n").unwrap();

        let json = run(tmp.path(), tmp.path(), "target", "renamed", false);
        // Both files get renamed — .rs via AST, .csv via text fallback
        // (word-boundary match: `target,` has a comma after, which is a
        // non-identifier char, so it matches).
        assert_eq!(json["files_modified"], 2);

        let csv = std::fs::read_to_string(tmp.path().join("data.csv")).unwrap();
        assert!(csv.contains("renamed,value"));
    }
}
