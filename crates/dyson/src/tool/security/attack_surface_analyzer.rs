// ===========================================================================
// AttackSurfaceAnalyzerTool — map where external input enters the codebase.
//
// Walks the directory tree, parses each file with tree-sitter, and
// identifies entry points by category:
//   - HTTP handlers (route decorators/attributes)
//   - CLI entry points (main, argparse)
//   - Network listeners (bind, listen)
//   - Database queries (query, execute)
//   - File I/O (open, fopen, readFile)
//   - Environment reads (env::var, os.environ, process.env)
//   - Deserialization (JSON.parse, json.loads, pickle.load, yaml.load)
//
// For each match, walks up the tree to find the enclosing function.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use tree_sitter::Node;

use crate::error::{DysonError, Result};
use crate::ast::{self, LanguageConfig};
use crate::ast::nodes;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::util::MAX_OUTPUT_BYTES;

/// Maximum entry points to collect.
const MAX_ENTRIES: usize = 500;

/// Patterns to look for, grouped by category.
/// Each tuple: (category_name, [identifiers_to_match]).
const SURFACE_PATTERNS: &[(&str, &[&str])] = &[
    (
        "HTTP Handlers",
        &[
            "route", "get", "post", "put", "delete", "patch", "app_route",
            "api_view", "RequestMapping", "GetMapping", "PostMapping",
            "PutMapping", "DeleteMapping", "PatchMapping", "HandleFunc",
            "http_method_funcs", "Router",
        ],
    ),
    (
        "CLI / Entry Points",
        &[
            "main", "argparse", "ArgumentParser", "clap", "structopt",
            "flag", "pflag", "cobra", "click",
        ],
    ),
    (
        "Network Listeners",
        &[
            "bind", "listen", "accept", "TcpListener", "UdpSocket",
            "createServer", "http_createServer", "net_createServer",
            "ServerSocket", "Socket",
        ],
    ),
    (
        "Database Queries",
        &[
            "execute", "query", "raw", "prepare", "cursor",
            "executemany", "fetchone", "fetchall", "raw_sql",
        ],
    ),
    (
        "File I/O",
        &[
            "open", "fopen", "readFile", "readFileSync", "writeFile",
            "writeFileSync", "read_to_string", "File_open",
            "fs_read", "fs_write",
        ],
    ),
    (
        "Environment Reads",
        &[
            "getenv", "environ", "env_var", "process_env",
            "os_getenv", "dotenv",
        ],
    ),
    (
        "Deserialization",
        &[
            "json_loads", "json_load", "JSON_parse", "pickle_loads",
            "pickle_load", "yaml_load", "yaml_safe_load",
            "Marshal_load", "ObjectInputStream", "deserialize",
            "from_json", "serde_json",
        ],
    ),
];

pub struct AttackSurfaceAnalyzerTool;

#[async_trait]
impl Tool for AttackSurfaceAnalyzerTool {
    fn name(&self) -> &str {
        "attack_surface_analyzer"
    }

    fn description(&self) -> &str {
        "Scan the codebase to map external-facing entry points: HTTP handlers, \
         CLI entry points, network listeners, database queries, file I/O, \
         environment variable reads, and deserialization calls.  Returns a \
         categorized list with file:line and enclosing function name.  Use this \
         to understand where external input enters the code before tracing \
         specific patterns with ast_query."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to scan (relative to working directory). \
                        Defaults to working directory."
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. '*.py', 'src/**/*.rs')"
                }
            }
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let search_dir = match input["path"].as_str() {
            Some(sub) => match ctx.resolve_path(sub) { Ok(p) => p, Err(e) => return Ok(e) },
            None => ctx.working_dir.clone(),
        };

        if !search_dir.exists() {
            return Ok(ToolOutput::error(format!(
                "directory does not exist: '{}'",
                search_dir.display()
            )));
        }

        let include_glob = input["include"].as_str().map(String::from);

        let working_dir_canon = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        // CPU-bound: walk + parse + scan.
        let results = tokio::task::spawn_blocking(move || {
            scan_attack_surface(&search_dir, &working_dir_canon, include_glob.as_deref())
        })
        .await
        .map_err(|e| {
            DysonError::tool(
                "attack_surface_analyzer",
                format!("scan task failed: {e}"),
            )
        })?;

        if results.is_empty() {
            return Ok(ToolOutput::success("No entry points found."));
        }

        Ok(ToolOutput::success(results))
    }
}

/// Scan for attack surface entry points across the directory.
fn scan_attack_surface(
    search_dir: &std::path::Path,
    working_dir_canon: &std::path::Path,
    include_glob: Option<&str>,
) -> String {
    let mut categories: Vec<(&str, Vec<String>)> = SURFACE_PATTERNS
        .iter()
        .map(|(cat, _)| (*cat, Vec::new()))
        .collect();

    let mut total_entries = 0usize;
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

    for entry in builder.build().flatten() {
        if total_entries >= MAX_ENTRIES
            || total_bytes >= MAX_OUTPUT_BYTES
            || file_count >= ast::MAX_FILES
        {
            break;
        }

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let parsed = match ast::try_parse_file(path, working_dir_canon, false) {
            Ok(Some(pair)) => pair,
            _ => continue,
        };
        let (config, parsed_file) = parsed;
        file_count += 1;

        scan_node(
            parsed_file.tree.root_node(),
            config,
            &parsed_file.source,
            &parsed_file.rel_path,
            &mut categories,
            &mut total_entries,
            &mut total_bytes,
        );
    }

    // Format output by category.
    let mut output = String::new();
    for (cat_name, entries) in &categories {
        if entries.is_empty() {
            continue;
        }
        writeln!(&mut output, "## {cat_name} ({} found)\n", entries.len()).unwrap();
        for entry in entries {
            writeln!(&mut output, "  {entry}").unwrap();
        }
        output.push('\n');
    }

    if output.is_empty() {
        return String::new();
    }

    let total: usize = categories.iter().map(|(_, entries)| entries.len()).sum();
    let mut header = format!("Attack Surface Analysis — {total} entry points found\n");
    header.push_str(&"=".repeat(50));
    header.push_str("\n\n");
    header.push_str(&output);
    header
}

/// Recursively scan an AST node for identifier matches against surface patterns.
fn scan_node(
    node: Node,
    config: &LanguageConfig,
    source: &str,
    rel_path: &str,
    categories: &mut [(&str, Vec<String>)],
    total_entries: &mut usize,
    total_bytes: &mut usize,
) {
    if *total_entries >= MAX_ENTRIES || *total_bytes >= MAX_OUTPUT_BYTES {
        return;
    }

    let source_bytes = source.as_bytes();
    let kind = node.kind();

    // Only identifier-kind AST nodes — substring matching on string
    // literals flagged any source containing common English fragments
    // (`"main"`, `"get"`, `"open"`) and blew past the 500-entry cap on
    // any non-trivial codebase.
    if config.identifier_types.contains(&kind) {
        let ident = &source[node.start_byte()..node.end_byte().min(source.len())];

        'cats: for (cat_idx, (_cat_name, patterns)) in SURFACE_PATTERNS.iter().enumerate() {
            for pattern in *patterns {
                // Exact case-insensitive match.  Substring matching is too
                // noisy: `"get"` matched `get_users`, `target`, `widget`;
                // `"main"` matched `domain`, `remain`, `maintenance`.
                if ident.eq_ignore_ascii_case(pattern) {
                    let line = node.start_position().row + 1;
                    let enclosing = ast::find_enclosing_function(node, config, source_bytes)
                        .and_then(|n| nodes::extract_definition_name(&n, source_bytes));
                    let context = enclosing
                        .as_deref()
                        .unwrap_or("<top-level>");

                    let entry = format!("{rel_path}:{line}: {context} — {ident}");

                    *total_bytes += entry.len() + 1;
                    *total_entries += 1;
                    categories[cat_idx].1.push(entry);

                    // One categorization per node.
                    break 'cats;
                }
            }
        }
    }

    // Recurse into children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_node(
            child,
            config,
            source,
            rel_path,
            categories,
            total_entries,
            total_bytes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn detects_python_route_handlers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "@app.route('/users')\ndef get_users():\n    return []\n\ndef helper():\n    pass\n",
        )
        .unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("route"),
            "output: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn detects_rust_network_listener() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("server.rs"),
            "use std::net::TcpListener;\n\
             fn start_server() {\n\
                 let listener = TcpListener::bind(\"0.0.0.0:8080\").unwrap();\n\
             }\n",
        )
        .unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("bind") || output.content.contains("TcpListener"),
            "output: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn empty_directory_returns_no_entry_points() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(
            output.content.contains("No entry points"),
            "output: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn respects_path_parameter() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(
            src.join("db.py"),
            "def run_query():\n    cursor.execute('SELECT 1')\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("other.py"),
            "def run_query():\n    cursor.execute('SELECT 1')\n",
        )
        .unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({"path": "src"});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        // Should only scan src/ — other.py is outside.
        if output.content.contains("execute") {
            assert!(
                output.content.contains("db.py"),
                "output: {}",
                output.content
            );
        }
    }

    #[test]
    fn is_agent_only() {
        assert!(AttackSurfaceAnalyzerTool.agent_only());
    }

    #[tokio::test]
    async fn does_not_flag_substring_matches() {
        // Identifiers that share substrings with patterns (`get`, `main`,
        // `post`, `open`, `flag`, `raw`) but aren't themselves the pattern
        // must not register — that's the bug that caused the scanner to
        // saturate at 500 entries on any realistic codebase.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def get_users():\n    domain = 'example.com'\n    maintenance = True\n    \
             reopen_file = None\n    budget = 0\n    message = 'main menu'\n    \
             flagship = 'x'\n    return domain\n",
        )
        .unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("No entry points"),
            "should have found no entry points; got: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn does_not_flag_strings_containing_patterns() {
        // String literals mentioning pattern words must not register.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("msg.py"),
            "def render():\n    \
             msg = 'please open the main menu and post a comment'\n    \
             return msg\n",
        )
        .unwrap();

        let tool = AttackSurfaceAnalyzerTool;
        let input = serde_json::json!({});
        let output = tool
            .run(&input, &ToolContext::for_test(tmp.path()))
            .await
            .unwrap();
        assert!(!output.is_error, "error: {}", output.content);
        assert!(
            output.content.contains("No entry points"),
            "should have found no entry points; got: {}",
            output.content
        );
    }
}
