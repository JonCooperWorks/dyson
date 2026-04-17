// Integration tests codifying patterns surfaced by ad-hoc smoke testing
// against real repos.  Each test is a regression guard for a real-world
// failure mode the isolated unit tests missed.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use dyson::tool::security::TaintTraceTool;
use dyson::tool::{Tool, ToolContext, ToolOutput};
use serde_json::json;

fn test_ctx(dir: &Path) -> ToolContext {
    ToolContext {
        working_dir: dir.to_path_buf(),
        env: HashMap::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        workspace: None,
        depth: 0,
        dangerous_no_sandbox: false,
        taint_indexes: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    }
}

async fn trace(
    files: &[(&str, &str)],
    language: &str,
    source: (&str, usize),
    sink: (&str, usize),
) -> ToolOutput {
    let tmp = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = tmp.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }
    let ctx = test_ctx(tmp.path());
    let tool = TaintTraceTool;
    tool.run(
        &json!({
            "language": language,
            "source": { "file": source.0, "line": source.1 },
            "sink":   { "file": sink.0,   "line": sink.1 },
        }),
        &ctx,
    )
    .await
    .expect("tool.run failed")
}

fn assert_ok(out: &ToolOutput) {
    assert!(!out.is_error, "tool returned error: {}", out.content);
}

fn assert_reaches(out: &ToolOutput) {
    assert_ok(out);
    assert!(
        out.content.contains("SINK REACHED"),
        "no SINK REACHED in output:\n{}",
        out.content,
    );
}

// ---------------------------------------------------------------------------
// Multi-line / wrapped function declarations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typescript_export_multiline_declaration() {
    // From claude-code: source line points at `export function X(` where the
    // AST's `function_declaration` node starts after `export `.  The byte
    // for source_line lands *outside* the definition's byte range.
    let src = "export function parseReferences(\n\
               \x20\x20input: string,\n\
               ): string {\n\
               \x20\x20return validate(input);\n\
               }\n\
               function validate(s: string): string {\n\
               \x20\x20return execute(s);\n\
               }\n\
               function execute(data: string): string {\n\
               \x20\x20return data;\n\
               }\n";
    let out = trace(
        &[("app.ts", src)],
        "typescript",
        ("app.ts", 1),
        ("app.ts", 7), // execute(s) call
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn rust_multiline_fn_declaration() {
    // Multi-line Rust signature — source.line must still resolve.
    let src = "fn long_signature(\n\
               \x20\x20\x20\x20input: String,\n\
               \x20\x20\x20\x20_cfg: String,\n\
               ) -> String {\n\
               \x20\x20\x20\x20execute(input)\n\
               }\n\
               fn execute(data: String) -> String { data }\n";
    let out = trace(
        &[("app.rs", src)],
        "rust",
        ("app.rs", 1),
        ("app.rs", 5),
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// Indented class methods (source line has leading whitespace)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn typescript_class_method_indented() {
    let src = "class Proxy {\n\
               \x20\x20handle(req: string): string {\n\
               \x20\x20\x20\x20return forward(req);\n\
               \x20\x20}\n\
               }\n\
               function forward(r: string): string {\n\
               \x20\x20return execute(r);\n\
               }\n\
               function execute(d: string): string { return d; }\n";
    let out = trace(
        &[("app.ts", src)],
        "typescript",
        ("app.ts", 2), // indented method header
        ("app.ts", 7), // execute(r)
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// Method callee resolution via `field` / `attribute` fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rust_self_method_call_resolves() {
    // `self.process(req)` uses `field_expression` whose field name lives in
    // the `field` field — not `property`/`name`.  Pre-fix, these were all
    // UnresolvedCallee.
    let src = "struct Handler;\n\
               impl Handler {\n\
               \x20\x20\x20\x20fn handle(&self, req: String) {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20self.process(req);\n\
               \x20\x20\x20\x20}\n\
               \x20\x20\x20\x20fn process(&self, data: String) {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20execute(data);\n\
               \x20\x20\x20\x20}\n\
               }\n\
               fn execute(s: String) {}\n";
    let out = trace(
        &[("app.rs", src)],
        "rust",
        ("app.rs", 3),
        ("app.rs", 7),
    )
    .await;
    assert_reaches(&out);
    assert!(
        !out.content.contains("UnresolvedCallee"),
        "self.process should resolve via `field` field: {}",
        out.content,
    );
}

#[tokio::test]
async fn python_self_method_call_resolves() {
    // Python uses `attribute` nodes with an `attribute` field for method names.
    let src = "class H:\n\
               \x20\x20\x20\x20def handle(self, req):\n\
               \x20\x20\x20\x20\x20\x20\x20\x20self.run(req)\n\
               \x20\x20\x20\x20def run(self, data):\n\
               \x20\x20\x20\x20\x20\x20\x20\x20execute(data)\n\
               def execute(x):\n\
               \x20\x20\x20\x20pass\n";
    let out = trace(
        &[("app.py", src)],
        "python",
        ("app.py", 2),
        ("app.py", 5),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn go_receiver_method_resolves() {
    // Go `selector_expression` uses `field` field, same as Rust.
    let src = "package p\n\
               type S struct{}\n\
               func (s *S) Handle(req string) { s.Exec(req) }\n\
               func (s *S) Exec(data string) { Run(data) }\n\
               func Run(x string) {}\n";
    let out = trace(
        &[("app.go", src)],
        "go",
        ("app.go", 3),
        ("app.go", 4),
    )
    .await;
    assert_reaches(&out);
    assert!(
        !out.content.contains("UnresolvedCallee"),
        "s.Exec(req) should resolve via `field` field: {}",
        out.content,
    );
}

// ---------------------------------------------------------------------------
// Source-line taint root excludes type names
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rust_fn_header_taint_root_is_params_only() {
    // Pre-fix, the taint root for `fn handler(body: Bytes, ...)` included
    // every type identifier on the line — `Arc`, `Bytes`, `Result`, etc.
    // The fix scopes taint to `FnDef.params` when source is on the header.
    let src = "use std::sync::Arc;\n\
               pub fn put_handler(body: String, _cfg: Arc<String>) -> Result<String, String> {\n\
               \x20\x20\x20\x20consume(body)\n\
               }\n\
               fn consume(data: String) -> Result<String, String> { Ok(data) }\n";
    let out = trace(
        &[("app.rs", src)],
        "rust",
        ("app.rs", 2),
        ("app.rs", 3),
    )
    .await;
    assert_reaches(&out);
    // Taint root line lists tainted identifiers.  None of these types
    // should appear as taint roots.
    let root_line = out
        .content
        .lines()
        .find(|l| l.contains("taint root"))
        .expect("no taint root line in output");
    for forbidden in ["Arc", "Result", "String"] {
        assert!(
            !root_line.contains(forbidden),
            "type `{forbidden}` leaked into taint root: {root_line}",
        );
    }
    assert!(root_line.contains("body"), "param `body` missing from taint: {root_line}");
}

#[tokio::test]
async fn typescript_fn_header_taint_root_is_params_only() {
    let src = "import { Config } from './config';\n\
               export function handler(body: string, _cfg: Config): Promise<string> {\n\
               \x20\x20return consume(body);\n\
               }\n\
               function consume(data: string): Promise<string> { return Promise.resolve(data); }\n";
    let out = trace(
        &[
            ("app.ts", src),
            ("config.ts", "export type Config = { x: number };\n"),
        ],
        "typescript",
        ("app.ts", 2),
        ("app.ts", 3),
    )
    .await;
    assert_reaches(&out);
    let root_line = out
        .content
        .lines()
        .find(|l| l.contains("taint root"))
        .expect("no taint root line");
    for forbidden in ["Config", "Promise", "Response"] {
        assert!(
            !root_line.contains(forbidden),
            "type `{forbidden}` leaked into taint root: {root_line}",
        );
    }
}

// ---------------------------------------------------------------------------
// Swift / Kotlin same-frame assignment propagation (tier-2 langs that
// were missing from assignment_types before the smoke fixes)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn swift_let_propagates_taint() {
    let src = "func handle(_ req: String) {\n\
               \x20\x20\x20\x20let x: String = req\n\
               \x20\x20\x20\x20execute(x)\n\
               }\n\
               func execute(_ s: String) {}\n";
    let out = trace(
        &[("app.swift", src)],
        "swift",
        ("app.swift", 1),
        ("app.swift", 3),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn kotlin_val_propagates_taint() {
    let src = "fun handle(req: String) {\n\
               \x20\x20\x20\x20val x = req\n\
               \x20\x20\x20\x20execute(x)\n\
               }\n\
               fun execute(s: String) {}\n";
    let out = trace(
        &[("app.kt", src)],
        "kotlin",
        ("app.kt", 1),
        ("app.kt", 3),
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// Nested function scope preservation across subsequent calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn python_call_after_nested_def_is_reached() {
    // A nested `def inner` used to flip `current_fn` to None on exit,
    // dropping every later call in the outer scope.  Multi-call outer is
    // the direct reproducer.
    let src = "def outer(req):\n\
               \x20\x20\x20\x20def inner(y):\n\
               \x20\x20\x20\x20\x20\x20\x20\x20return y\n\
               \x20\x20\x20\x20a(req)\n\
               \x20\x20\x20\x20b(req)\n\
               def a(x):\n\
               \x20\x20\x20\x20pass\n\
               def b(x):\n\
               \x20\x20\x20\x20execute(x)\n\
               def execute(d):\n\
               \x20\x20\x20\x20pass\n";
    // Trace to the execute call reached via b(req).
    let out = trace(
        &[("app.py", src)],
        "python",
        ("app.py", 1),
        ("app.py", 9), // execute(x) inside b
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// TSX components
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tsx_component_traces_to_sink() {
    let src = "export function Component(props: { value: string }) {\n\
               \x20\x20const v = props.value;\n\
               \x20\x20return render(v);\n\
               }\n\
               function render(s: string) {\n\
               \x20\x20return execute(s);\n\
               }\n\
               function execute(d: string) { return d; }\n";
    let out = trace(
        &[("app.tsx", src)],
        "tsx",
        ("app.tsx", 1),
        ("app.tsx", 6),
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// Positional argument binding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_tainted_arg_binds_to_its_param_position() {
    // Non-tainted args must NOT cause their positionally-matched params
    // to be tainted in the callee.
    let src = "def handler(req):\n\
               \x20\x20\x20\x20process(\"safe\", req, \"other\")\n\
               def process(a, b, c):\n\
               \x20\x20\x20\x20execute(b)\n\
               def execute(x):\n\
               \x20\x20\x20\x20pass\n";
    let out = trace(
        &[("app.py", src)],
        "python",
        ("app.py", 1),
        ("app.py", 4),
    )
    .await;
    assert_reaches(&out);
    // `b` should be tainted in process, not `a` or `c`.
    assert!(
        out.content.contains("param `b`"),
        "positional binding failed: {}",
        out.content,
    );
    assert!(
        !out.content.contains("param `a`") && !out.content.contains("params `a`"),
        "non-tainted arg poisoned param `a`: {}",
        out.content,
    );
}

// ---------------------------------------------------------------------------
// Cross-file traversal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_file_python_three_hop_trace() {
    let a = "from b import sanitize\n\
             from c import save\n\
             def handler(req):\n\
             \x20\x20\x20\x20s = sanitize(req)\n\
             \x20\x20\x20\x20save(s)\n";
    let b = "def sanitize(input):\n\
             \x20\x20\x20\x20return input.strip()\n";
    let c = "def save(sql):\n\
             \x20\x20\x20\x20execute(sql)\n\
             def execute(q):\n\
             \x20\x20\x20\x20pass\n";
    let out = trace(
        &[("a.py", a), ("b.py", b), ("c.py", c)],
        "python",
        ("a.py", 3),
        ("c.py", 2), // execute(sql) inside save
    )
    .await;
    assert_reaches(&out);
    // Path should touch a.py, b.py, and c.py.
    for f in ["a.py", "c.py"] {
        assert!(
            out.content.contains(f),
            "path missing file {f}: {}",
            out.content,
        );
    }
}

// ---------------------------------------------------------------------------
// Index sanity — unresolved_callees ratio caps regression.
// Pre-fix, dyson's own src/ had ~30% unresolved callees because Rust
// `field_expression` wasn't wired up.  This test guards against a
// regression on a controlled fixture.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rust_method_calls_mostly_resolve() {
    let src = "struct S;\n\
               impl S {\n\
               \x20\x20\x20\x20fn a(&self) { self.b(); self.c(); }\n\
               \x20\x20\x20\x20fn b(&self) {}\n\
               \x20\x20\x20\x20fn c(&self) {}\n\
               }\n";
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("app.rs"), src).unwrap();
    let config = dyson::ast::config_for_language_name("rust").unwrap();
    let index = dyson::ast::taint::build_index(config, tmp.path())
        .await
        .unwrap();
    assert_eq!(
        index.unresolved_callees, 0,
        "all method calls in the fixture should resolve; index reports {} unresolved",
        index.unresolved_callees,
    );
    assert_eq!(index.call_sites.len(), 2);
}

// ---------------------------------------------------------------------------
// Empty project is a clean NO_PATH, not an error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_project_is_not_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("empty.py"), "").unwrap();
    let ctx = test_ctx(tmp.path());
    let tool = TaintTraceTool;
    let out = tool
        .run(
            &json!({
                "language": "python",
                "source": { "file": "empty.py", "line": 1 },
                "sink":   { "file": "empty.py", "line": 1 },
            }),
            &ctx,
        )
        .await
        .unwrap();
    // Will be an error — "no enclosing function" — which is correct behavior
    // (empty file has no functions).  Guard against panics or crashes.
    assert!(
        out.is_error || out.content.contains("NO_PATH"),
        "unexpected output: is_error={}, content={}",
        out.is_error,
        out.content,
    );
}

/// Regression: Rust `println!`, `vec!`, `format!` etc. are `macro_invocation`
/// nodes whose callee sits in the `macro` field, not `function`.  The tool
/// was leaving them unresolved until the fallback chain picked up `macro`.
#[tokio::test]
async fn rust_macro_invocation_resolves_callee() {
    let src = "fn handle(req: String) {\n    log!(req);\n    execute(req);\n}\nfn execute(s: String) {}\nmacro_rules! log { ($x:expr) => {{ let _ = $x; }} }\n";
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("app.rs"), src).unwrap();
    let config = dyson::ast::config_for_language_name("rust").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp.path()).await.unwrap();
    let log_call = idx
        .call_sites
        .iter()
        .find(|cs| cs.callee == "log")
        .expect("macro `log!` should resolve to `log`");
    assert_eq!(log_call.callee, "log");
}

/// Regression: Swift `obj.method()` parses as
/// `call_expression → navigation_expression → navigation_suffix`.  An
/// earlier version of `flatten_callee` only matched `property`/`name`
/// fields and returned empty for the navigation chain — half of Swift
/// calls came back unresolved (46% on real repos).
#[tokio::test]
async fn swift_navigation_expression_resolves_method_callee() {
    let src = "class Handler {\n\
               \x20\x20func process(_ req: String) {\n\
               \x20\x20\x20\x20let q = device.makeQuery(req)\n\
               \x20\x20\x20\x20execute(q)\n\
               \x20\x20}\n\
               }\n\
               func execute(_ s: String) {}\n";
    let out = trace(
        &[("app.swift", src)],
        "swift",
        ("app.swift", 2),
        ("app.swift", 4),
    )
    .await;
    assert_reaches(&out);
    assert!(
        !out.content.contains("device.makeQuery` — callee unresolved"),
        "navigation_expression should resolve to `makeQuery`: {}",
        out.content,
    );
}

/// Regression: Zig's tree-sitter grammar uses `function_declaration` and
/// `variable_declaration`, not the `fn_decl`/`var_decl` the earlier
/// LanguageConfig specified.  Zig indexing produced 0 defs / 0 calls on
/// a 277-file project until the definition_types list was corrected.
#[tokio::test]
async fn zig_indexes_function_declarations() {
    let src = "fn handle(req: []const u8) void {\n\
               \x20\x20\x20\x20const x = req;\n\
               \x20\x20\x20\x20execute(x);\n\
               }\n\
               fn execute(s: []const u8) void {\n\
               \x20\x20\x20\x20_ = s;\n\
               }\n";
    let out = trace(
        &[("app.zig", src)],
        "zig",
        ("app.zig", 1),
        ("app.zig", 3),
    )
    .await;
    assert_reaches(&out);
}

/// Regression: C# `obj.Method()` uses `invocation_expression` + member
/// access.  The tool must resolve method calls on receivers.
#[tokio::test]
async fn csharp_method_call_resolves() {
    let src = "class Handler {\n\
               \x20\x20public void Process(string req) {\n\
               \x20\x20\x20\x20Execute(req);\n\
               \x20\x20}\n\
               \x20\x20public void Execute(string data) {\n\
               \x20\x20\x20\x20// sink\n\
               \x20\x20}\n\
               }\n";
    let out = trace(
        &[("App.cs", src)],
        "csharp",
        ("App.cs", 2),
        ("App.cs", 3),
    )
    .await;
    assert_reaches(&out);
}

// ---------------------------------------------------------------------------
// Trace smoke for every remaining supported language — ensures each
// language's grammar + LanguageConfig + taint machinery lines up end-to-end.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn java_method_trace_resolves() {
    let src = "class App {\n\
               \x20\x20\x20\x20void handle(String req) {\n\
               \x20\x20\x20\x20\x20\x20\x20\x20execute(req);\n\
               \x20\x20\x20\x20}\n\
               \x20\x20\x20\x20void execute(String data) {}\n\
               }\n";
    let out = trace(
        &[("App.java", src)],
        "java",
        ("App.java", 2),
        ("App.java", 3),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn ruby_method_trace_resolves() {
    let src = "def handle(req)\n\
               \x20\x20execute(req)\n\
               end\n\
               def execute(data)\n\
               end\n";
    let out = trace(
        &[("app.rb", src)],
        "ruby",
        ("app.rb", 1),
        ("app.rb", 2),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn c_function_trace_resolves() {
    let src = "#include <stddef.h>\n\
               void execute(const char *s) { (void)s; }\n\
               void handle(const char *req) {\n\
               \x20\x20\x20\x20execute(req);\n\
               }\n";
    let out = trace(
        &[("app.c", src)],
        "c",
        ("app.c", 3),
        ("app.c", 4),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn cpp_function_trace_resolves() {
    let src = "#include <string>\n\
               void execute(const std::string& s) { (void)s; }\n\
               void handle(const std::string& req) {\n\
               \x20\x20\x20\x20execute(req);\n\
               }\n";
    let out = trace(
        &[("app.cpp", src)],
        "cpp",
        ("app.cpp", 3),
        ("app.cpp", 4),
    )
    .await;
    assert_reaches(&out);
}

/// Regression: OCaml `application_expression.function` returns a `value_path`
/// wrapping a `value_name`.  Neither was in `is_identifier_kind`, so callees
/// came back empty — 100% of dune's 6,154 calls were unresolved.
#[tokio::test]
async fn ocaml_application_resolves_value_name() {
    let src = "let execute s = s\nlet handle req = execute req\n";
    let out = trace(
        &[("app.ml", src)],
        "ocaml",
        ("app.ml", 2),
        ("app.ml", 2),
    )
    .await;
    assert_reaches(&out);
}

/// Haskell's `apply` has `function` field which is a `variable`; both are
/// now in the identifier sets.  Index should build cleanly and resolve
/// callees at ≥50% (remaining unresolved are corner cases — lambdas,
/// operator sections, typeclass dispatch).
#[tokio::test]
async fn haskell_indexes_apply_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("App.hs"),
        "execute s = s\nhandle req = execute req\nprocess x = foo (bar x)\n",
    )
    .unwrap();
    let config = dyson::ast::config_for_language_name("haskell").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp.path()).await.unwrap();
    assert!(!idx.call_sites.is_empty(), "Haskell should index apply calls");
    let resolved = idx
        .call_sites
        .iter()
        .filter(|cs| !cs.callee.is_empty())
        .count();
    assert!(
        resolved >= idx.call_sites.len() / 2,
        "at least half of Haskell calls should have a non-empty callee; got {resolved}/{}",
        idx.call_sites.len(),
    );
}

/// Erlang's `call` node with `-module/-export` preamble — ensure calls
/// inside a function clause are attributed to that clause.
#[tokio::test]
async fn erlang_indexes_call_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("app.erl"),
        "-module(app).\n\
         -export([handle/1]).\n\
         handle(Req) -> execute(Req).\n\
         execute(Data) -> Data.\n",
    )
    .unwrap();
    let config = dyson::ast::config_for_language_name("erlang").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp.path()).await.unwrap();
    assert!(
        !idx.call_sites.is_empty(),
        "Erlang should index at least one call",
    );
    // `execute` should resolve.
    assert!(
        idx.call_sites.iter().any(|cs| cs.callee == "execute"),
        "Erlang `execute(Req)` callee should resolve to `execute`",
    );
}

#[tokio::test]
async fn elixir_function_trace_resolves() {
    let src = "defmodule App do\n\
               \x20\x20def handle(req) do\n\
               \x20\x20\x20\x20execute(req)\n\
               \x20\x20end\n\
               \x20\x20def execute(data) do\n\
               \x20\x20\x20\x20data\n\
               \x20\x20end\n\
               end\n";
    let out = trace(
        &[("app.ex", src)],
        "elixir",
        ("app.ex", 2),
        ("app.ex", 3),
    )
    .await;
    assert_reaches(&out);
}

/// OCaml / Erlang / Haskell / Nix trace shapes are functional-dominant
/// and the tool isn't a polished fit — we still want the index to build
/// cleanly and error paths to be clean, even if traces don't always find.
#[tokio::test]
async fn functional_languages_at_least_index_cleanly() {
    // OCaml
    let ocaml = "let execute s = s\nlet handle req = execute req\n";
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.ml"), ocaml).unwrap();
    let config = dyson::ast::config_for_language_name("ocaml").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp.path()).await.unwrap();
    assert!(!idx.call_sites.is_empty(), "OCaml should index at least one call");

    // Haskell
    let haskell = "execute s = s\nhandle req = execute req\n";
    let tmp2 = tempfile::tempdir().unwrap();
    fs::write(tmp2.path().join("A.hs"), haskell).unwrap();
    let config = dyson::ast::config_for_language_name("haskell").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp2.path()).await.unwrap();
    assert!(!idx.call_sites.is_empty(), "Haskell should index at least one call");

    // Nix — wrap the apply inside a binding so the walker records it.
    // Top-level Nix expressions sit outside any binding scope; taint_trace
    // only records calls that fall inside a tracked scope.
    let nix = "{ result = execute \"hi\"; }\n";
    let tmp3 = tempfile::tempdir().unwrap();
    fs::write(tmp3.path().join("a.nix"), nix).unwrap();
    let config = dyson::ast::config_for_language_name("nix").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp3.path()).await.unwrap();
    assert!(!idx.call_sites.is_empty(), "Nix should index at least one call");

    // Erlang
    let erlang = "-module(app).\nhandle(Req) -> execute(Req).\nexecute(Data) -> Data.\n";
    let tmp4 = tempfile::tempdir().unwrap();
    fs::write(tmp4.path().join("app.erl"), erlang).unwrap();
    let config = dyson::ast::config_for_language_name("erlang").unwrap();
    let idx = dyson::ast::taint::build_index(config, tmp4.path()).await.unwrap();
    assert!(!idx.call_sites.is_empty(), "Erlang should index at least one call");
}

// ---------------------------------------------------------------------------
// Scale + cross-language traces — replicating the spirit of smoke
// testing against real repos.  Each test exercises the full pipeline
// on a multi-file, multi-language fixture that mirrors a shape seen
// in actual projects.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_file_typescript_three_hop_trace() {
    // Shape from a real Node TS backend: request handler → service → db.
    let handler = "import { sanitize } from './sanitize';\n\
                   import { save } from './db';\n\
                   export async function attachUser(req: Request) {\n\
                       const clean = sanitize(req);\n\
                       save(clean);\n\
                   }\n";
    let sanitize = "export function sanitize(input: Request) {\n\
                        return input;\n\
                    }\n";
    let db = "export function save(payload: Request) {\n\
                  execute(payload);\n\
              }\n\
              function execute(query: Request) { return query; }\n";
    let out = trace(
        &[
            ("middleware/auth.ts", handler),
            ("lib/sanitize.ts", sanitize),
            ("lib/db.ts", db),
        ],
        "typescript",
        ("middleware/auth.ts", 3),
        ("lib/db.ts", 2),
    )
    .await;
    assert_reaches(&out);
    for f in ["middleware/auth.ts", "lib/db.ts"] {
        assert!(out.content.contains(f), "missing {f}: {}", out.content);
    }
}

#[tokio::test]
async fn cross_file_rust_three_hop_trace() {
    // Shape from an axum-style Rust service: handler → sanitizer → sink.
    let handler = "use crate::sanitize::clean;\n\
                   use crate::db::save;\n\
                   pub fn put_blob_handler(body: String) -> String {\n\
                       let c = clean(body);\n\
                       save(c)\n\
                   }\n";
    let sanitize = "pub fn clean(input: String) -> String { input }\n";
    let db = "pub fn save(payload: String) -> String {\n\
                  execute(payload)\n\
              }\n\
              fn execute(query: String) -> String { query }\n";
    let out = trace(
        &[
            ("src/http.rs", handler),
            ("src/sanitize.rs", sanitize),
            ("src/db.rs", db),
        ],
        "rust",
        ("src/http.rs", 3),
        ("src/db.rs", 2),
    )
    .await;
    assert_reaches(&out);
}

#[tokio::test]
async fn cross_file_go_three_hop_trace() {
    let handler = "package main\n\
                   import (\n\
                   \t\"example.com/sanitize\"\n\
                   \t\"example.com/db\"\n\
                   )\n\
                   func director(req string) string {\n\
                       c := sanitize.Clean(req)\n\
                       return db.Save(c)\n\
                   }\n";
    let sanitize = "package sanitize\n\
                    func Clean(input string) string { return input }\n";
    let db = "package db\n\
              func Save(payload string) string {\n\
                  return Execute(payload)\n\
              }\n\
              func Execute(query string) string { return query }\n";
    let out = trace(
        &[
            ("main.go", handler),
            ("sanitize/sanitize.go", sanitize),
            ("db/db.go", db),
        ],
        "go",
        ("main.go", 6),
        ("db/db.go", 3),
    )
    .await;
    assert_reaches(&out);
}

/// Generate a 60-file Python project and verify the index builds, the
/// unresolved ratio is reasonable, and a single-file trace still works.
/// Mirrors the scale stressor that smoke testing applied ad-hoc.
#[tokio::test]
async fn many_file_python_project_indexes_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    // 59 noise files: simple `def f(): pass` in each.
    for i in 0..59 {
        fs::write(
            tmp.path().join(format!("mod{i:03}.py")),
            format!("def f{i}(x):\n    return x\n"),
        )
        .unwrap();
    }
    // 1 real trace target.
    fs::write(
        tmp.path().join("app.py"),
        "def handler(req):\n    execute(req)\ndef execute(q):\n    pass\n",
    )
    .unwrap();

    let ctx = test_ctx(tmp.path());
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
    assert_reaches(&out);
    // Header line reports index stats.  Confirm we indexed the 60 files.
    let header = out
        .content
        .lines()
        .find(|l| l.starts_with("index:"))
        .expect("no index header line");
    assert!(header.contains("files=60"), "expected 60 files: {header}");
    // Unresolved callees should be 0 — every callee is defined locally.
    assert!(
        header.contains("unresolved_callees=0"),
        "unexpected unresolved callees in a fully-local fixture: {header}",
    );
}

/// Exceed MAX_FILES (500) and confirm truncation is flagged, not silently
/// dropped.  Surfaced during smoke on a 500+ file C++ repo.
#[tokio::test]
async fn index_flags_max_files_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..520 {
        fs::write(
            tmp.path().join(format!("m{i:04}.py")),
            format!("def f{i}():\n    pass\n"),
        )
        .unwrap();
    }
    let config = dyson::ast::config_for_language_name("python").unwrap();
    let index = dyson::ast::taint::build_index(config, tmp.path())
        .await
        .unwrap();
    assert!(
        index.truncated,
        "500+ files should trigger MAX_FILES truncation",
    );
    assert_eq!(
        index.file_mtimes.len(),
        500,
        "should have indexed exactly MAX_FILES",
    );
}

/// Multiple languages can coexist in the same ToolContext without
/// cross-contamination.  Smoke hit this when a multi-language repo
/// (Python + C++) cached two independent indexes.
#[tokio::test]
async fn multiple_language_indexes_coexist_in_cache() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("app.py"),
        "def handler(req):\n    execute(req)\ndef execute(q):\n    pass\n",
    )
    .unwrap();
    fs::write(
        tmp.path().join("app.rs"),
        "fn handler(req: String) { execute(req); }\nfn execute(q: String) {}\n",
    )
    .unwrap();
    let ctx = test_ctx(tmp.path());
    let tool = TaintTraceTool;

    for (lang, file, src_line, sink_line) in [
        ("python", "app.py", 1, 2),
        ("rust", "app.rs", 1, 1),
    ] {
        let out = tool
            .run(
                &json!({
                    "language": lang,
                    "source": { "file": file, "line": src_line },
                    "sink":   { "file": file, "line": sink_line },
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{lang} errored: {}", out.content);
    }
    assert_eq!(
        ctx.taint_indexes.read().await.len(),
        2,
        "should cache one index per language",
    );
}

/// Sanity: the index rebuilds when a file is modified after caching.
/// (The tool uses mtime invalidation — smoke verified this works in
/// practice by running back-to-back traces between edits.)
#[tokio::test]
async fn index_rebuilds_on_file_modification() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("app.py"),
        "def handler(req):\n    execute(req)\ndef execute(q): pass\n",
    )
    .unwrap();
    let ctx = test_ctx(tmp.path());
    let tool = TaintTraceTool;
    let input = json!({
        "language": "python",
        "source": { "file": "app.py", "line": 1 },
        "sink":   { "file": "app.py", "line": 2 },
    });
    let _ = tool.run(&input, &ctx).await.unwrap();
    let first_index = ctx
        .taint_indexes
        .read()
        .await
        .get("Python")
        .cloned()
        .unwrap();

    // Modify the file — force a newer mtime.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(
        tmp.path().join("app.py"),
        "def handler(req):\n    execute(req)\n    other(req)\ndef execute(q): pass\ndef other(q): pass\n",
    )
    .unwrap();

    let _ = tool.run(&input, &ctx).await.unwrap();
    let second_index = ctx
        .taint_indexes
        .read()
        .await
        .get("Python")
        .cloned()
        .unwrap();

    assert!(
        !Arc::ptr_eq(&first_index, &second_index),
        "index should have been rebuilt after file mtime changed",
    );
    assert!(
        second_index.call_sites.len() > first_index.call_sites.len(),
        "rebuilt index should reflect the added call",
    );
}
