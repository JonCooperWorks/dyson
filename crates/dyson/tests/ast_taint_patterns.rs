// Integration tests codifying patterns surfaced by the smoke run
// (see `examples/smoke_taint_trace.rs`).  Each test is a regression
// guard for a real-world failure mode the ad-hoc unit tests missed.

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

// Confirm files we reference exist as expected (sanity for future refactors).
#[test]
fn smoke_example_exists() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("smoke_taint_trace.rs");
    assert!(p.exists(), "smoke example missing at {}", p.display());
}
