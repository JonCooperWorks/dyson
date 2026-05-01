// Smoke-test ast_query against real open-source repos.  Exercises the
// documented P95 query patterns from `security_engineer.md` across every
// language we ship a grammar for — catches grammar-version drift (a
// tree-sitter bump that renames `scoped_identifier` to `path_expression`,
// say) before the security_engineer agent hits it at runtime.  Every bug
// surfaced here graduates to a deterministic unit test in
// `tool/security/ast_query.rs`.
//
// Self-contained: on first run, shallow-clones each target repo into
// `$TMPDIR/dyson-smoke-repos/` (shared with the other smoke tests).
// Subsequent runs reuse the clones.  Expect ~2 GB disk and ~5 min
// first-run wall time; ~30 s thereafter.
//
// Run with:
//     cargo run -p dyson --example smoke_ast_query --release

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use dyson::ast;
use dyson::tool::security::ast_query::execute_query_string;

struct Target {
    slug: &'static str,
    sub: &'static str,
    language: &'static str,
    /// Canonical queries taken from `security_engineer.md` (or derived
    /// from the same grammar conventions).  Every query MUST compile;
    /// we also record whether it produced any matches as a sanity check
    /// that the node types actually exist in this grammar.
    queries: &'static [(&'static str, &'static str)],
}

fn main() {
    let cache = std::env::temp_dir().join("dyson-smoke-repos");
    std::fs::create_dir_all(&cache).expect("create cache dir");

    let targets: &[Target] = &[
        Target {
            slug: "tokio-rs/tokio",
            sub: "tokio/src",
            language: "rust",
            queries: &[
                (
                    "function_item",
                    "(function_item name: (identifier) @fn_name) @fn",
                ),
                ("call_expression", "(call_expression) @call"),
                (
                    "scoped_identifier-fields",
                    "(scoped_identifier path: (identifier) @path name: (identifier) @name) @si",
                ),
                (
                    "field_expression-fields",
                    "(field_expression value: (_) @value field: (field_identifier) @field) @fe",
                ),
                (
                    "macro_invocation-macro",
                    "(macro_invocation macro: [(identifier) (scoped_identifier)] @macro) @mac",
                ),
                ("unsafe_block", "(unsafe_block) @unsafe"),
            ],
        },
        Target {
            slug: "BurntSushi/ripgrep",
            sub: "crates",
            language: "rust",
            queries: &[
                ("function_item", "(function_item name: (identifier) @fn) @_"),
                ("call_expression", "(call_expression) @call"),
                (
                    "struct_item",
                    "(struct_item name: (type_identifier) @name) @_",
                ),
            ],
        },
        Target {
            slug: "gin-gonic/gin",
            sub: "",
            language: "go",
            queries: &[
                (
                    "function_declaration",
                    "(function_declaration name: (identifier) @fn) @_",
                ),
                ("call_expression", "(call_expression) @call"),
                (
                    "selector_expression",
                    "(selector_expression operand: (_) @obj field: (field_identifier) @field) @se",
                ),
            ],
        },
        Target {
            slug: "syncthing/syncthing",
            sub: "lib",
            language: "go",
            queries: &[
                ("function_declaration", "(function_declaration) @fn"),
                (
                    "method_declaration",
                    "(method_declaration name: (field_identifier) @name) @_",
                ),
            ],
        },
        Target {
            slug: "django/django",
            sub: "django",
            language: "python",
            queries: &[
                (
                    "function_definition",
                    "(function_definition name: (identifier) @fn) @_",
                ),
                ("call", "(call function: (_) @target) @call"),
                (
                    "attribute",
                    "(attribute object: (_) @obj attribute: (identifier) @attr) @_",
                ),
                (
                    "argument_list",
                    "(call arguments: (argument_list) @args) @_",
                ),
            ],
        },
        Target {
            slug: "pallets/flask",
            sub: "src/flask",
            language: "python",
            queries: &[
                (
                    "decorated_definition",
                    "(decorated_definition (decorator) @dec definition: (function_definition name: (identifier) @name)) @_",
                ),
                (
                    "eval-family",
                    "(call function: (identifier) @fn (#match? @fn \"^(eval|exec|compile)$\")) @_",
                ),
            ],
        },
        Target {
            slug: "psf/requests",
            sub: "src",
            language: "python",
            queries: &[
                (
                    "class_definition",
                    "(class_definition name: (identifier) @name) @_",
                ),
                (
                    "pickle-family",
                    "(call function: (attribute object: (identifier) @mod (#match? @mod \"^(pickle|yaml|marshal)$\"))) @_",
                ),
            ],
        },
        Target {
            slug: "microsoft/TypeScript",
            sub: "src",
            language: "typescript",
            queries: &[
                ("call_expression", "(call_expression) @call"),
                (
                    "member_expression",
                    "(member_expression object: (_) @obj property: (property_identifier) @prop) @_",
                ),
                (
                    "function_declaration",
                    "(function_declaration name: (identifier) @name) @_",
                ),
            ],
        },
        Target {
            slug: "nestjs/nest",
            sub: "packages",
            language: "typescript",
            queries: &[
                ("class_declaration", "(class_declaration) @cls"),
                ("decorator", "(decorator) @dec"),
            ],
        },
        Target {
            slug: "vercel/next.js",
            sub: "packages/next/src",
            language: "tsx",
            queries: &[
                (
                    "dangerouslySetInnerHTML",
                    "(jsx_attribute (property_identifier) @attr (#eq? @attr \"dangerouslySetInnerHTML\")) @_",
                ),
                ("jsx_element", "(jsx_element) @jsx"),
            ],
        },
        Target {
            slug: "expressjs/express",
            sub: "lib",
            language: "javascript",
            queries: &[
                (
                    "call-identifier",
                    "(call_expression function: (identifier) @fn) @_",
                ),
                (
                    "member_expression",
                    "(member_expression object: (_) @obj property: (property_identifier) @prop) @_",
                ),
            ],
        },
        Target {
            slug: "lodash/lodash",
            sub: "",
            language: "javascript",
            queries: &[
                ("arrow_function", "(arrow_function) @fn"),
                ("function_declaration", "(function_declaration) @fn"),
            ],
        },
        Target {
            slug: "rails/rails",
            sub: "activerecord/lib",
            language: "ruby",
            queries: &[
                ("method", "(method name: (identifier) @name) @_"),
                ("class", "(class name: (constant) @name) @_"),
            ],
        },
        Target {
            slug: "spring-projects/spring-petclinic",
            sub: "",
            language: "java",
            queries: &[
                (
                    "method_declaration",
                    "(method_declaration name: (identifier) @name) @_",
                ),
                ("annotation", "(annotation name: (identifier) @name) @_"),
                (
                    "method_invocation",
                    "(method_invocation name: (identifier) @name) @_",
                ),
            ],
        },
        Target {
            slug: "git/git",
            sub: "",
            language: "c",
            queries: &[
                (
                    "function_definition",
                    "(function_definition declarator: (function_declarator declarator: (identifier) @name)) @_",
                ),
                ("call_expression", "(call_expression) @call"),
                ("preproc_include", "(preproc_include) @inc"),
            ],
        },
        Target {
            slug: "redis/redis",
            sub: "src",
            language: "c",
            queries: &[
                ("function_definition", "(function_definition) @fn"),
                ("call_expression", "(call_expression) @call"),
            ],
        },
        Target {
            slug: "nlohmann/json",
            sub: "include",
            language: "cpp",
            queries: &[
                ("function_definition", "(function_definition) @fn"),
                ("call_expression", "(call_expression) @call"),
            ],
        },
        Target {
            slug: "Kotlin/kotlinx.coroutines",
            sub: "kotlinx-coroutines-core/jvm/src",
            language: "kotlin",
            queries: &[
                ("function_declaration", "(function_declaration) @fn"),
                ("call_expression", "(call_expression) @call"),
            ],
        },
        Target {
            slug: "apple/swift-nio",
            sub: "Sources",
            language: "swift",
            queries: &[
                ("function_declaration", "(function_declaration) @fn"),
                ("call_expression", "(call_expression) @call"),
            ],
        },
        Target {
            slug: "elixir-lang/elixir",
            sub: "lib",
            language: "elixir",
            queries: &[("call", "(call) @call")],
        },
        Target {
            slug: "ninenines/cowboy",
            sub: "",
            language: "erlang",
            queries: &[("function_clause", "(function_clause) @fn")],
        },
        Target {
            slug: "jgm/pandoc",
            sub: "",
            language: "haskell",
            queries: &[("function", "(function) @fn")],
        },
        Target {
            slug: "dotnet/samples",
            sub: "",
            language: "csharp",
            queries: &[
                ("invocation_expression", "(invocation_expression) @call"),
                (
                    "method_declaration",
                    "(method_declaration name: (identifier) @name) @_",
                ),
            ],
        },
        Target {
            slug: "nix-darwin/nix-darwin",
            sub: "modules",
            language: "nix",
            queries: &[("attrset_expression", "(attrset_expression) @attrs")],
        },
    ];

    let mut totals = Totals::default();

    // Compile-check pass: verify every documented query compiles.  This
    // runs without the clones and is the fastest canary for grammar
    // drift.  Compilation failure is a hard error.
    println!("═══ query compile-check pass ═══");
    for t in targets {
        let config = match ast::config_for_language_name(t.language) {
            Some(c) => c,
            None => {
                totals.errors += 1;
                eprintln!("  [{}] unknown language", t.language);
                continue;
            }
        };
        for (label, q) in t.queries {
            totals.queries_attempted += 1;
            match tree_sitter::Query::new(&config.language, q) {
                Ok(query) => {
                    if query.capture_names().is_empty() {
                        totals.errors += 1;
                        eprintln!("  [{} / {}] COMPILE-OK but no captures", t.language, label);
                    } else {
                        totals.queries_compiled += 1;
                    }
                }
                Err(e) => {
                    totals.errors += 1;
                    eprintln!("  [{} / {}] COMPILE-FAIL: {e}", t.language, label);
                }
            }
        }
    }
    println!(
        "  compiled: {}/{} queries",
        totals.queries_compiled, totals.queries_attempted
    );

    // Escape hatch: run only the in-process compile pass, no clones.
    // Useful on CI or when validating a grammar bump without network.
    if std::env::var("DYSON_SMOKE_COMPILE_ONLY").is_ok() {
        println!("\nDYSON_SMOKE_COMPILE_ONLY set — skipping clone + execute phase.");
        if totals.errors > 0 {
            std::process::exit(1);
        }
        return;
    }

    // Execute pass: run each query against the cloned repo.  Zero matches
    // is NOT an error — some patterns legitimately have no hits on clean
    // production code — but we do report the count so a run can be sanity
    // eyeballed.
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
        "\n═══ summary ═══\nrepos: {} | clones-failed: {} | errors: {} | queries-compiled: {}/{} | queries-executed: {} | queries-with-matches: {}",
        targets.len(),
        totals.clone_errors,
        totals.errors,
        totals.queries_compiled,
        totals.queries_attempted,
        totals.queries_executed,
        totals.queries_with_matches,
    );
    if totals.errors > 0 || totals.clone_errors > 0 {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Totals {
    errors: usize,
    clone_errors: usize,
    queries_attempted: usize,
    queries_compiled: usize,
    queries_executed: usize,
    queries_with_matches: usize,
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

    let started = Instant::now();
    for (label, q) in t.queries {
        totals.queries_executed += 1;
        match execute_query_string(q, config, path, None) {
            Ok(results) => {
                if !results.is_empty() {
                    totals.queries_with_matches += 1;
                }
                println!("  {label}: {} matches", results.len());
            }
            Err(e) => {
                totals.errors += 1;
                eprintln!("  {label}: EXEC-FAIL: {e}");
            }
        }
    }
    println!("  total elapsed: {:.2}s", started.elapsed().as_secs_f32());
    Ok(())
}
