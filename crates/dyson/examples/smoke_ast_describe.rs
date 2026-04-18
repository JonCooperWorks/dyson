// Smoke-test ast_describe against real open-source repos.  Stress-tests
// the parser + tree walker + line-range narrowing against production code
// across every language we ship a grammar for.  Every bug surfaced here
// graduates to a deterministic unit test in
// `tool/security/ast_describe.rs`.
//
// Self-contained: on first run, shallow-clones each target repo into
// `$TMPDIR/dyson-smoke-repos/` (shared with the other smoke tests).
// Subsequent runs reuse the clones.  Expect ~2 GB disk and ~5 min
// first-run wall time; ~30 s thereafter.
//
// Run with:
//     cargo run -p dyson --example smoke_ast_describe --release

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use dyson::ast;
use dyson::tool::security::ast_describe::describe_source;

struct Target {
    slug: &'static str,
    sub: &'static str,
    language: &'static str,
    /// A minimal snippet in this language whose node tree MUST contain
    /// all of `expect_kinds` after rendering.  Catches grammar-name drift
    /// on every run.
    snippet: &'static str,
    expect_kinds: &'static [&'static str],
    /// Glob of files to sample from the cloned repo for path-mode runs.
    file_ext: &'static str,
}

fn main() {
    let cache = std::env::temp_dir().join("dyson-smoke-repos");
    std::fs::create_dir_all(&cache).expect("create cache dir");

    let targets: &[Target] = &[
        Target {
            slug: "tokio-rs/tokio",
            sub: "tokio/src",
            language: "rust",
            snippet: "fn f() { Command::new(\"ls\").spawn(); }",
            expect_kinds: &["call_expression", "scoped_identifier", "field_expression"],
            file_ext: "rs",
        },
        Target {
            slug: "BurntSushi/ripgrep",
            sub: "crates",
            language: "rust",
            snippet: "fn g() { vec![1, 2, 3]; }",
            expect_kinds: &["macro_invocation"],
            file_ext: "rs",
        },
        Target {
            slug: "gin-gonic/gin",
            sub: "",
            language: "go",
            snippet: "package p\nfunc F() { fmt.Println(\"hi\") }",
            expect_kinds: &["call_expression", "selector_expression"],
            file_ext: "go",
        },
        Target {
            slug: "syncthing/syncthing",
            sub: "lib",
            language: "go",
            snippet: "package p\nfunc F(r *http.Request) { r.URL.Query() }",
            expect_kinds: &["call_expression", "selector_expression"],
            file_ext: "go",
        },
        Target {
            slug: "django/django",
            sub: "django",
            language: "python",
            snippet: "import json\njson.loads(payload)\n",
            expect_kinds: &["call", "attribute", "argument_list"],
            file_ext: "py",
        },
        Target {
            slug: "pallets/flask",
            sub: "src/flask",
            language: "python",
            snippet: "@app.route('/')\ndef home():\n    return 'hi'\n",
            expect_kinds: &["decorated_definition", "function_definition"],
            file_ext: "py",
        },
        Target {
            slug: "psf/requests",
            sub: "src",
            language: "python",
            snippet: "class A:\n    def m(self): pass\n",
            expect_kinds: &["class_definition", "function_definition"],
            file_ext: "py",
        },
        Target {
            slug: "microsoft/TypeScript",
            sub: "src",
            language: "typescript",
            snippet: "function f(x: number): number { return x.toString().length; }",
            expect_kinds: &["call_expression", "member_expression"],
            file_ext: "ts",
        },
        Target {
            slug: "nestjs/nest",
            sub: "packages",
            language: "typescript",
            snippet: "class X { @Get() method() {} }",
            expect_kinds: &["decorator"],
            file_ext: "ts",
        },
        Target {
            slug: "vercel/next.js",
            sub: "packages/next/src",
            language: "tsx",
            snippet: "const X = () => <div dangerouslySetInnerHTML={{__html: x}} />",
            expect_kinds: &["jsx_self_closing_element", "jsx_attribute"],
            file_ext: "tsx",
        },
        Target {
            slug: "expressjs/express",
            sub: "lib",
            language: "javascript",
            snippet: "app.get('/', (req, res) => res.send('hi'));",
            expect_kinds: &["call_expression", "member_expression"],
            file_ext: "js",
        },
        Target {
            slug: "lodash/lodash",
            sub: "",
            language: "javascript",
            snippet: "function f(x) { return x.map(y => y + 1); }",
            expect_kinds: &["arrow_function", "member_expression"],
            file_ext: "js",
        },
        Target {
            slug: "rails/rails",
            sub: "activerecord/lib",
            language: "ruby",
            snippet: "class A\n  def m\n    'hi'\n  end\nend\n",
            expect_kinds: &["class", "method"],
            file_ext: "rb",
        },
        Target {
            slug: "sinatra/sinatra",
            sub: "",
            language: "ruby",
            snippet: "get '/' do\n  'hi'\nend\n",
            // `get '/' do ... end` parses as a `call` with `method: (identifier)`
            // and a `do_block`.  There is no `method_call` node in tree-sitter-ruby
            // for this bareword-with-block form.
            expect_kinds: &["call", "do_block"],
            file_ext: "rb",
        },
        Target {
            slug: "spring-projects/spring-petclinic",
            sub: "",
            language: "java",
            snippet: "class A { @GetMapping(\"/x\") String m() { return \"hi\"; } }",
            expect_kinds: &["annotation", "method_declaration"],
            file_ext: "java",
        },
        Target {
            slug: "junit-team/junit5",
            sub: "junit-jupiter-api/src/main/java",
            language: "java",
            snippet: "class A { void m() { Foo.bar(); } }",
            expect_kinds: &["method_invocation"],
            file_ext: "java",
        },
        Target {
            slug: "git/git",
            sub: "",
            language: "c",
            snippet: "int main() { printf(\"%s\", x); return 0; }",
            expect_kinds: &["call_expression", "function_definition"],
            file_ext: "c",
        },
        Target {
            slug: "redis/redis",
            sub: "src",
            language: "c",
            snippet: "#include <stdio.h>\nint f(char *s) { return strlen(s); }",
            expect_kinds: &["preproc_include", "call_expression"],
            file_ext: "c",
        },
        Target {
            slug: "DaveGamble/cJSON",
            sub: "",
            language: "c",
            snippet: "int main(int argc, char **argv) { return argc; }",
            expect_kinds: &["function_definition"],
            file_ext: "c",
        },
        Target {
            slug: "nlohmann/json",
            sub: "include",
            language: "cpp",
            // Use `printf` so we get a guaranteed call_expression; the
            // `std::cout << "hi"` form parses as a binary_expression, which
            // is a real parse but not the node type we're asserting.
            snippet: "int main() { printf(\"hi\"); return 0; }",
            expect_kinds: &["call_expression", "function_definition"],
            file_ext: "hpp",
        },
        Target {
            slug: "Kotlin/kotlinx.coroutines",
            sub: "kotlinx-coroutines-core/jvm/src",
            language: "kotlin",
            snippet: "fun main() { println(\"hi\") }",
            expect_kinds: &["call_expression", "function_declaration"],
            file_ext: "kt",
        },
        Target {
            slug: "apple/swift-nio",
            sub: "Sources",
            language: "swift",
            snippet: "func f() { print(\"hi\") }",
            expect_kinds: &["call_expression", "function_declaration"],
            file_ext: "swift",
        },
        Target {
            slug: "ziglang/zig",
            sub: "src",
            language: "zig",
            snippet: "pub fn main() void { std.debug.print(\"hi\", .{}); }",
            // Current tree-sitter-zig uses `function_declaration` / `field_expression`;
            // older vocab (`FnProto`, `FieldAccessExpr`) is gone.
            expect_kinds: &["function_declaration", "field_expression", "call_expression"],
            file_ext: "zig",
        },
        Target {
            slug: "elixir-lang/elixir",
            sub: "lib",
            language: "elixir",
            snippet: "defmodule M do\n  def f(x), do: x\nend\n",
            expect_kinds: &["call"],
            file_ext: "ex",
        },
        Target {
            slug: "ninenines/cowboy",
            sub: "",
            language: "erlang",
            snippet: "-module(m).\n-export([f/1]).\nf(X) -> X + 1.\n",
            expect_kinds: &["function_clause"],
            file_ext: "erl",
        },
        Target {
            slug: "jgm/pandoc",
            sub: "",
            language: "haskell",
            snippet: "f :: Int -> Int\nf x = x + 1\n",
            expect_kinds: &["signature", "function"],
            file_ext: "hs",
        },
        Target {
            slug: "ocaml/dune",
            sub: "",
            language: "ocaml",
            snippet: "let f x = x + 1\n",
            expect_kinds: &["value_definition"],
            file_ext: "ml",
        },
        Target {
            slug: "dotnet/samples",
            sub: "",
            language: "csharp",
            snippet: "class A { void M() { System.Console.WriteLine(\"hi\"); } }",
            expect_kinds: &["invocation_expression"],
            file_ext: "cs",
        },
        Target {
            slug: "nix-darwin/nix-darwin",
            sub: "modules",
            language: "nix",
            snippet: "{ x = 1; y = x + 1; }\n",
            expect_kinds: &["attrset_expression"],
            file_ext: "nix",
        },
    ];

    let mut totals = Totals::default();

    // Snippet-mode pass: same check in-process, no clones.  This is the
    // fastest canary for grammar drift and runs even when offline.
    println!("═══ snippet-mode sanity pass ═══");
    for t in targets {
        totals.snippets_attempted += 1;
        match check_snippet(t) {
            Ok(()) => totals.snippets_ok += 1,
            Err(e) => {
                totals.errors += 1;
                eprintln!("  [{}] SNIPPET-FAIL: {}", t.language, e);
            }
        }
    }
    println!(
        "  snippets: {}/{} ok",
        totals.snippets_ok, totals.snippets_attempted
    );

    // Escape hatch: run only the in-process snippet pass, no clones.
    // Useful on CI or when validating a grammar bump without network.
    if std::env::var("DYSON_SMOKE_COMPILE_ONLY").is_ok() {
        println!(
            "\nDYSON_SMOKE_COMPILE_ONLY set — skipping clone + path-mode phase."
        );
        if totals.errors > 0 {
            std::process::exit(1);
        }
        return;
    }

    // Path-mode pass: real files from real repos.
    for t in targets {
        let repo_dir = cache.join(repo_dirname(t.slug));
        if !repo_dir.exists() {
            println!("\n→ cloning {} …", t.slug);
            if let Err(e) = shallow_clone(t.slug, &repo_dir) {
                eprintln!("  clone error: {e}");
                totals.clone_errors += 1;
                continue;
            }
        }
        let path = if t.sub.is_empty() {
            repo_dir.clone()
        } else {
            repo_dir.join(t.sub)
        };
        let label = if t.sub.is_empty() {
            t.slug.to_string()
        } else {
            format!("{}/{}", t.slug, t.sub)
        };
        println!("\n═══ {label} [{}] @ {} ═══", t.language, path.display());
        if !path.exists() {
            eprintln!("  subpath missing; skipping");
            continue;
        }
        match run_repo(t, &path, &mut totals) {
            Ok(()) => {}
            Err(e) => {
                totals.errors += 1;
                eprintln!("  ERROR: {e}");
            }
        }
    }

    println!(
        "\n═══ summary ═══\nrepos: {} | clones-failed: {} | errors: {} | snippets: {}/{} | files-rendered: {} | line-ranges-narrowed: {}",
        targets.len(),
        totals.clone_errors,
        totals.errors,
        totals.snippets_ok,
        totals.snippets_attempted,
        totals.files_rendered,
        totals.line_ranges_narrowed,
    );
    if totals.errors > 0 || totals.clone_errors > 0 {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Totals {
    errors: usize,
    clone_errors: usize,
    snippets_attempted: usize,
    snippets_ok: usize,
    files_rendered: usize,
    line_ranges_narrowed: usize,
}

fn check_snippet(t: &Target) -> Result<(), String> {
    let config = ast::config_for_language_name(t.language)
        .ok_or_else(|| format!("unknown language: {}", t.language))?;
    let out = describe_source(t.snippet, config, None, 20)
        .map_err(|e| format!("describe: {e}"))?;
    for expected in t.expect_kinds {
        if !out.contains(expected) {
            return Err(format!(
                "expected '{expected}' in rendered tree for snippet:\n---\n{}\n---\ngot:\n{out}",
                t.snippet
            ));
        }
    }
    Ok(())
}

fn repo_dirname(slug: &str) -> String {
    slug.replace('/', "__")
}

fn shallow_clone(slug: &str, dest: &Path) -> Result<(), String> {
    let url = format!("https://github.com/{slug}.git");
    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--quiet", &url])
        .arg(dest)
        .status()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !status.success() {
        return Err(format!("git clone {url} exited {status}"));
    }
    Ok(())
}

fn run_repo(t: &Target, path: &Path, totals: &mut Totals) -> Result<(), String> {
    let config = ast::config_for_language_name(t.language)
        .ok_or_else(|| format!("unknown language: {}", t.language))?;

    // Sample up to 5 files matching the extension, in repo walk order.
    let sample = sample_files(path, t.file_ext, 5);
    if sample.is_empty() {
        eprintln!("  no .{} files found; skipping", t.file_ext);
        return Ok(());
    }

    let started = Instant::now();
    let mut rendered_bytes = 0usize;
    for file in &sample {
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if source.is_empty() {
            continue;
        }

        // Full-tree render.
        let out = describe_source(&source, config, None, 20)
            .map_err(|e| format!("full-tree {}: {e}", file.display()))?;
        if out.is_empty() {
            return Err(format!("empty output for {}", file.display()));
        }
        rendered_bytes += out.len();
        totals.files_rendered += 1;

        // Line-range render: pick a 5-line window in the middle of the file
        // and verify the narrowed output is shorter than the full output.
        let line_count = source.lines().count();
        if line_count >= 20 {
            let mid = line_count / 2;
            let range_start = mid.saturating_sub(2).max(1);
            let range_end = (mid + 2).min(line_count);
            let narrowed = describe_source(
                &source,
                config,
                Some((range_start, range_end)),
                20,
            )
            .map_err(|e| format!("line-range {}: {e}", file.display()))?;
            if narrowed.len() < out.len() {
                totals.line_ranges_narrowed += 1;
            }
        }
    }
    let elapsed = started.elapsed();
    println!(
        "  rendered {} files, {:.1} KB total, {:.2}s",
        sample.len(),
        rendered_bytes as f32 / 1024.0,
        elapsed.as_secs_f32()
    );
    Ok(())
}

fn sample_files(root: &Path, ext: &str, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(limit);
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(false).git_ignore(true);
    for entry in builder.build().flatten() {
        if out.len() >= limit {
            break;
        }
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(p.to_path_buf());
        }
    }
    out
}
