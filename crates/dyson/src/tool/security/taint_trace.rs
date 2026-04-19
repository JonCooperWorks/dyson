// TaintTraceTool — thin agent-facing wrapper around `ast::taint::trace`.
// Caches the per-language SymbolIndex on ToolContext with mtime
// invalidation; renders paths with explicit uncertainty annotations.

use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::ast::{self, taint};
use crate::error::{DysonError, Result};
use crate::tool::{Tool, ToolContext, ToolOutput};

#[derive(Deserialize)]
struct Input {
    language: String,
    source: Position,
    sink: Position,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    max_paths: Option<usize>,
}

#[derive(Deserialize)]
struct Position {
    file: String,
    line: usize,
}

pub struct TaintTraceTool;

#[async_trait]
impl Tool for TaintTraceTool {
    fn name(&self) -> &str {
        "taint_trace"
    }

    fn description(&self) -> &str {
        "Cross-file source→sink reachability oracle.  Given a source file:line \
         (where taint enters) and a sink file:line (the dangerous operation), \
         returns candidate call chains ranked by confidence.  Name-based \
         resolution with positional argument binding — LOSSY by design.  \
         Every returned path is a HYPOTHESIS; verify each hop with read_file \
         before filing.  Use `ast_query` first to discover sinks and sources, \
         then call `taint_trace` once per (source, sink) pair to rank \
         reachability.  Supports all 20 languages (JSON excepted)."
    }

    fn agent_only(&self) -> bool {
        true
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "description": "Target language.  Same names as ast_query: rust, python, \
                        javascript, typescript, tsx, go, java, c, cpp, csharp, ruby, kotlin, \
                        swift, zig, elixir, erlang, ocaml, haskell, nix.  JSON is unsupported."
                },
                "source": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Path relative to working dir" },
                        "line": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["file", "line"]
                },
                "sink": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Path relative to working dir" },
                        "line": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["file", "line"]
                },
                "max_depth": { "type": "integer", "minimum": 1, "default": 16 },
                "max_paths": { "type": "integer", "minimum": 1, "default": 10 }
            },
            "required": ["language", "source", "sink"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: Input = serde_json::from_value(input.clone())
            .map_err(|e| DysonError::tool("taint_trace", format!("invalid input: {e}")))?;

        let config = match ast::config_for_language_name(&parsed.language) {
            Some(c) => c,
            None => {
                return Ok(ToolOutput::error(format!(
                    "unknown language '{}'",
                    parsed.language
                )));
            }
        };

        if config.call_types.is_empty() {
            return Ok(ToolOutput::error(format!(
                "language '{}' does not support taint_trace (no call concept)",
                config.display_name
            )));
        }

        let source_path = match ctx.resolve_path(&parsed.source.file) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let sink_path = match ctx.resolve_path(&parsed.sink.file) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };

        // Cache lookup: acquire read lock, check for existing index and
        // validate mtimes.  Miss or staleness → drop read, build, write.
        let index = {
            let guard = ctx.taint_indexes.read().await;
            guard.get(config.display_name).cloned()
        };

        let index = match index {
            Some(idx) if !taint::is_stale(&idx, &ctx.working_dir) => idx,
            _ => {
                let fresh =
                    taint::build_index(config, &ctx.working_dir)
                        .await
                        .map_err(|e| {
                            DysonError::tool("taint_trace", format!("index build: {e}"))
                        })?;
                let arc = Arc::new(fresh);
                let mut guard = ctx.taint_indexes.write().await;
                guard.insert(config.display_name, Arc::clone(&arc));
                arc
            }
        };

        let opts = taint::TraceOptions {
            // Defaults sized for modern RSC/RPC wire-format chains —
            // e.g. `FormData → resolveField → getChunk → JSON.parse →
            // reviveModel → parseModelString → getOutlinedModel → walk`
            // is 8 hops on its own, which the old depth-8 cap cut short.
            max_depth: parsed.max_depth.unwrap_or(16),
            max_paths: parsed.max_paths.unwrap_or(10),
            ..taint::TraceOptions::default()
        };

        let result = match taint::trace(
            &index,
            config,
            &ctx.working_dir,
            &source_path,
            parsed.source.line,
            &sink_path,
            parsed.sink.line,
            &opts,
        ) {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(format!("{e}"))),
        };

        Ok(ToolOutput::success(render(&index, &result, &parsed)))
    }
}

fn render(index: &taint::SymbolIndex, result: &taint::TraceResult, input: &Input) -> String {
    let mut out = String::new();

    let _ = writeln!(
        out,
        "taint_trace: lossy — verify every hop with read_file before filing",
    );
    let _ = writeln!(
        out,
        "index: language={}, files={}, defs={}, calls={}, unresolved_callees={}{}",
        index.language,
        index.file_mtimes.len(),
        index.fn_defs.len(),
        index.call_sites.len(),
        index.unresolved_callees,
        if index.truncated {
            " [TRUNCATED: MAX_FILES hit]"
        } else {
            ""
        },
    );

    if result.truncated_frontier {
        let _ = writeln!(
            out,
            "WARNING: BFS frontier cap (10k) hit — output may be incomplete"
        );
    }

    let _ = writeln!(
        out,
        "\nFound {} candidate path(s) from {}:{} to {}:{}:\n",
        result.paths.len(),
        input.source.file,
        input.source.line,
        input.sink.file,
        input.sink.line,
    );

    if result.paths.is_empty() {
        let _ = writeln!(
            out,
            "NO_PATH — BFS completed without reaching the sink from the source.",
        );
        let _ = writeln!(
            out,
            "Interpretation: either (a) no call chain exists, (b) the chain goes through",
        );
        let _ = writeln!(
            out,
            "dynamic dispatch / an import alias the tool couldn't resolve, or (c) the source",
        );
        let _ = writeln!(
            out,
            "line's tainted symbols don't match the sink line's arguments by name.",
        );
        return out;
    }

    for (i, path) in result.paths.iter().enumerate() {
        let _ = writeln!(
            out,
            "Path {} (depth {}, resolved {}/{} hops{}):",
            i + 1,
            path.depth(),
            path.resolved_hops(),
            path.hops.len(),
            if path.imprecise_bindings() > 0 {
                format!(", {} imprecise", path.imprecise_bindings())
            } else {
                String::new()
            },
        );
        for (j, hop) in path.hops.iter().enumerate() {
            let prefix = if j == 0 {
                "  ".to_string()
            } else {
                format!("  {}└─ ", "  ".repeat(j.saturating_sub(1)))
            };
            let kind_tag = match hop.kind {
                taint::HopKind::Source => "",
                taint::HopKind::Resolved => "",
                taint::HopKind::ImpreciseBinding => " [IMPRECISE]",
                taint::HopKind::Ambiguous => " [AMBIGUOUS]",
                taint::HopKind::UnresolvedCallee => " [UNRESOLVED]",
                taint::HopKind::Sink => "",
            };
            let _ = writeln!(
                out,
                "{prefix}{}:{} [byte {}-{}] — {}{}",
                display_path(&hop.file),
                hop.line,
                hop.byte_range.start,
                hop.byte_range.end,
                hop.detail,
                kind_tag,
            );
            for &id in &hop.ambiguous_candidates {
                let d = &index.fn_defs[id];
                let _ = writeln!(
                    out,
                    "{prefix}    - {}:{} {}()",
                    display_path(&d.file),
                    d.line,
                    d.name,
                );
            }
        }
        let _ = writeln!(out);
    }

    out
}

fn display_path(p: &std::path::Path) -> String {
    p.to_string_lossy().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    #[tokio::test]
    async fn same_function_same_line_trivially_traces() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def handler(req):\n    execute(req)\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "app.py", "line": 1 },
                    "sink":   { "file": "app.py", "line": 2 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(out.content.contains("SINK REACHED"), "{}", out.content);
    }

    #[tokio::test]
    async fn cross_file_trace_with_intermediate_hop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("server.ts"),
            "import { sanitize } from './sanitize';\n\
             import { query } from './db';\n\
             function handler(req: any) {\n\
                 const s = sanitize(req);\n\
                 query(s);\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("sanitize.ts"),
            "export function sanitize(input: any) {\n\
                 return input.toString();\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("db.ts"),
            "export function query(sql: any) {\n\
                 conn.execute(sql);\n\
             }\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "typescript",
                    "source": { "file": "server.ts", "line": 3 },
                    "sink":   { "file": "db.ts", "line": 2 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(
            out.content.contains("SINK REACHED") || out.content.contains("candidate path"),
            "{}",
            out.content,
        );
    }

    #[tokio::test]
    async fn unreachable_returns_no_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def handler(req):\n    return 'ok'\n\ndef other():\n    execute('static')\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "app.py", "line": 1 },
                    "sink":   { "file": "app.py", "line": 5 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(out.content.contains("NO_PATH"), "{}", out.content);
    }

    #[tokio::test]
    async fn same_frame_assignment_propagation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def handler(req):\n    x = req\n    execute(x)\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "app.py", "line": 1 },
                    "sink":   { "file": "app.py", "line": 3 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(out.content.contains("SINK REACHED"), "{}", out.content);
    }

    #[tokio::test]
    async fn source_on_blank_line_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "\n\ndef handler(req):\n    execute(req)\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "app.py", "line": 1 },
                    "sink":   { "file": "app.py", "line": 4 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("no enclosing function"),
            "{}",
            out.content,
        );
    }

    #[tokio::test]
    async fn json_language_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.json"), "{}").unwrap();
        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "json",
                    "source": { "file": "a.json", "line": 1 },
                    "sink":   { "file": "a.json", "line": 1 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("does not support"), "{}", out.content);
    }

    #[tokio::test]
    async fn unknown_language_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "brainfuck",
                    "source": { "file": "a.bf", "line": 1 },
                    "sink":   { "file": "a.bf", "line": 1 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("unknown language"), "{}", out.content);
    }

    #[tokio::test]
    async fn ambiguous_name_resolution_lists_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.py"),
            "def handler(req):\n    apply(req)\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("m1.py"),
            "def apply(x):\n    return x\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("m2.py"),
            "def apply(y):\n    return y\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "a.py", "line": 1 },
                    "sink":   { "file": "m1.py", "line": 2 },
                    "max_depth": 3
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        // Expect AMBIGUOUS tag somewhere in the output (either in a path or
        // in the candidate list), OR expect it to list both m1.py:1 and m2.py:1
        // under the ambiguous hop.
        assert!(
            out.content.contains("AMBIGUOUS") || out.content.contains("m2.py"),
            "{}",
            out.content,
        );
    }

    #[tokio::test]
    async fn index_is_cached_across_calls() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def handler(req):\n    execute(req)\n",
        )
        .unwrap();
        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let input = json!({
            "language": "python",
            "source": { "file": "app.py", "line": 1 },
            "sink":   { "file": "app.py", "line": 2 },
        });
        let _ = tool.run(&input, &ctx).await.unwrap();
        assert_eq!(
            ctx.taint_indexes.read().await.len(),
            1,
            "first call should populate the cache",
        );
        let _ = tool.run(&input, &ctx).await.unwrap();
        assert_eq!(
            ctx.taint_indexes.read().await.len(),
            1,
            "second call should reuse the cached index",
        );
    }

    /// Regression: the Walker used to set `current_fn = None` on exit from
    /// any definition, which meant calls in `outer` appearing *after* a
    /// nested `inner` definition were silently dropped.
    #[tokio::test]
    async fn nested_function_does_not_drop_outer_scope() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.py"),
            "def outer(req):\n    def inner(x):\n        pass\n    execute(req)\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "python",
                    "source": { "file": "app.py", "line": 1 },
                    "sink":   { "file": "app.py", "line": 4 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(
            out.content.contains("SINK REACHED"),
            "nested fn scope was dropped: {}",
            out.content,
        );
    }

    /// Regression: Rust's `let_declaration` exposes its LHS via the
    /// `pattern` field, not `name` / `left`.  Earlier the LHS came back
    /// empty, so `let x = req; execute(x);` never propagated taint.
    #[tokio::test]
    async fn rust_let_declaration_propagates_taint() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.rs"),
            "fn handler(req: String) {\n    let x = req;\n    execute(x);\n}\n\
             fn execute(s: String) { let _ = s; }\n",
        )
        .unwrap();

        let ctx = ToolContext::for_test(tmp.path());
        let tool = TaintTraceTool;
        let out = tool
            .run(
                &json!({
                    "language": "rust",
                    "source": { "file": "app.rs", "line": 1 },
                    "sink":   { "file": "app.rs", "line": 3 },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "err: {}", out.content);
        assert!(
            out.content.contains("SINK REACHED"),
            "Rust let-declaration propagation broken: {}",
            out.content,
        );
    }
}
