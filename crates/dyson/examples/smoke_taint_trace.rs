// Smoke-test taint_trace against real open-source repos.  Stress-tests
// the parser + index + BFS end-to-end on production code — every bug
// surfaced here graduates to a deterministic regression test in
// `tests/ast_taint_patterns.rs`.
//
// Self-contained: on first run, shallow-clones each target repo into
// `$TMPDIR/dyson-smoke-repos/`.  Subsequent runs reuse the clones.
// Expect ~2 GB disk and ~5 min first-run wall time; ~30 s thereafter.
//
// Run with:
//     cargo run -p dyson --example smoke_taint_trace --release

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use dyson::ast::{self, taint};

struct Target {
    /// `org/repo` on github.com.
    slug: &'static str,
    /// Subpath inside the repo; empty = whole repo.
    sub: &'static str,
    language: &'static str,
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run());
}

async fn run() {
    let cache = std::env::temp_dir().join("dyson-smoke-repos");
    std::fs::create_dir_all(&cache).expect("create cache dir");

    let targets: &[Target] = &[
        // Rust (generics, macros, async, traits)
        Target { slug: "tokio-rs/tokio", sub: "tokio/src", language: "rust" },
        Target { slug: "BurntSushi/ripgrep", sub: "crates", language: "rust" },
        Target { slug: "bevyengine/bevy", sub: "crates", language: "rust" },
        Target { slug: "denoland/deno", sub: "cli", language: "rust" },
        Target { slug: "nushell/nushell", sub: "crates", language: "rust" },
        Target { slug: "clap-rs/clap", sub: "", language: "rust" },
        // Go (structs, interfaces, receivers)
        Target { slug: "gin-gonic/gin", sub: "", language: "go" },
        Target { slug: "syncthing/syncthing", sub: "lib", language: "go" },
        // Python (classes, metaclasses, decorators)
        Target { slug: "django/django", sub: "django", language: "python" },
        Target { slug: "pallets/flask", sub: "src/flask", language: "python" },
        Target { slug: "psf/requests", sub: "src", language: "python" },
        // TypeScript
        Target { slug: "microsoft/TypeScript", sub: "src", language: "typescript" },
        Target { slug: "facebook/react", sub: "packages", language: "typescript" },
        Target { slug: "nestjs/nest", sub: "packages", language: "typescript" },
        // TSX (React components)
        Target { slug: "vercel/next.js", sub: "packages/next/src", language: "tsx" },
        // JavaScript
        Target { slug: "expressjs/express", sub: "lib", language: "javascript" },
        Target { slug: "lodash/lodash", sub: "", language: "javascript" },
        // Ruby (metaprogramming, open classes)
        Target { slug: "rails/rails", sub: "activerecord/lib", language: "ruby" },
        Target { slug: "sinatra/sinatra", sub: "", language: "ruby" },
        Target { slug: "jekyll/jekyll", sub: "lib", language: "ruby" },
        // Java (annotations, generics)
        Target { slug: "spring-projects/spring-petclinic", sub: "", language: "java" },
        Target { slug: "junit-team/junit5", sub: "junit-jupiter-api/src/main/java", language: "java" },
        // C (K&R-style + modern)
        Target { slug: "git/git", sub: "", language: "c" },
        Target { slug: "redis/redis", sub: "src", language: "c" },
        Target { slug: "DaveGamble/cJSON", sub: "", language: "c" },
        Target { slug: "curl/curl", sub: "lib", language: "c" },
        // C++ (templates, inheritance, operator overloading)
        Target { slug: "SerenityOS/serenity", sub: "Kernel", language: "cpp" },
        Target { slug: "nlohmann/json", sub: "include", language: "cpp" },
        // Kotlin
        Target { slug: "Kotlin/kotlinx.coroutines", sub: "kotlinx-coroutines-core/jvm/src", language: "kotlin" },
        Target { slug: "square/okhttp", sub: "okhttp-coroutines/src/main/kotlin", language: "kotlin" },
        // Swift (generics, protocols, property wrappers)
        Target { slug: "apple/swift-nio", sub: "Sources", language: "swift" },
        Target { slug: "apple/swift-collections", sub: "Sources", language: "swift" },
        Target { slug: "vapor/vapor", sub: "Sources", language: "swift" },
        // Zig (comptime, anytype)
        Target { slug: "ziglang/zig", sub: "src", language: "zig" },
        Target { slug: "zigtools/zls", sub: "src", language: "zig" },
        // Elixir (pattern matching, macros)
        Target { slug: "elixir-lang/elixir", sub: "lib", language: "elixir" },
        Target { slug: "elixir-plug/plug", sub: "", language: "elixir" },
        // Erlang (pattern matching, OTP)
        Target { slug: "ninenines/cowboy", sub: "", language: "erlang" },
        // Haskell (typeclasses, GADTs, HKT)
        Target { slug: "jgm/pandoc", sub: "", language: "haskell" },
        // OCaml (functors, variants)
        Target { slug: "ocaml/dune", sub: "", language: "ocaml" },
        // C# (LINQ, async, generics)
        Target { slug: "dotnet/samples", sub: "", language: "csharp" },
        // Nix (packaging expressions — where real taint would live)
        Target { slug: "nix-darwin/nix-darwin", sub: "modules", language: "nix" },
    ];

    let mut totals = Totals::default();

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
        match run_repo(t.language, &path, &mut totals).await {
            Ok(()) => {}
            Err(e) => {
                totals.errors += 1;
                eprintln!("  ERROR: {e}");
            }
        }
    }

    println!(
        "\n═══ summary ═══\nrepos: {} | clones-failed: {} | trace-errors: {} | traces-attempted: {} | traces-with-path: {} | index-errors: {}",
        targets.len(),
        totals.clone_errors,
        totals.errors,
        totals.traces_attempted,
        totals.traces_with_path,
        totals.index_errors,
    );
    if totals.errors > 0 || totals.index_errors > 0 || totals.clone_errors > 0 {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Totals {
    errors: usize,
    clone_errors: usize,
    traces_attempted: usize,
    traces_with_path: usize,
    index_errors: usize,
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

async fn run_repo(
    language: &'static str,
    path: &Path,
    totals: &mut Totals,
) -> Result<(), String> {
    let config = ast::config_for_language_name(language)
        .ok_or_else(|| format!("unknown language: {language}"))?;

    let started = Instant::now();
    let index = match taint::build_index(config, path).await {
        Ok(i) => i,
        Err(e) => {
            totals.index_errors += 1;
            return Err(format!("build_index: {e}"));
        }
    };
    let elapsed = started.elapsed();
    let unresolved_pct = if index.call_sites.is_empty() {
        0.0
    } else {
        index.unresolved_callees as f32 / index.call_sites.len() as f32 * 100.0
    };
    let confidence =
        taint::Confidence::from_unresolved_ratio(index.unresolved_callees, index.call_sites.len());
    // Count assignments that exercise field-path precision (dotted LHS
    // or RHS paths) — lets us spot corpora where the new path collector
    // earns its keep vs. corpora where everything still degenerates to
    // bare identifiers.
    let field_assigns = index
        .assignments
        .iter()
        .filter(|a| {
            a.lhs.iter().any(|s| s.contains('.'))
                || a.rhs_idents.iter().any(|s| s.contains('.'))
        })
        .count();
    println!(
        "  index: {} files, {} defs, {} calls, {} assigns ({} field-path), {} unresolved ({:.0}%, max_confidence={}), build {:.2}s{}",
        index.file_mtimes.len(),
        index.fn_defs.len(),
        index.call_sites.len(),
        index.assignments.len(),
        field_assigns,
        index.unresolved_callees,
        unresolved_pct,
        confidence.as_str(),
        elapsed.as_secs_f32(),
        if index.truncated { " [TRUNCATED]" } else { "" },
    );

    for (label, picker) in &[
        ("req→sink", pick_req_to_sink as PickFn),
        ("config→write", pick_config_to_write),
        ("first-fn→first-call", pick_first_to_first),
    ] {
        if let Some(probe) = picker(&index) {
            totals.traces_attempted += 1;
            match run_probe(&index, config, path, probe).await {
                Ok(true) => {
                    totals.traces_with_path += 1;
                    println!("  {label}: PATH");
                }
                Ok(false) => println!("  {label}: no-path"),
                Err(e) => {
                    totals.errors += 1;
                    eprintln!("  {label}: TRACE-ERROR: {e}");
                }
            }
        }
    }
    Ok(())
}

type PickFn = fn(&taint::SymbolIndex) -> Option<Probe>;

struct Probe {
    source_fn: usize,
    sink_call: usize,
}

async fn run_probe(
    index: &taint::SymbolIndex,
    config: &'static ast::LanguageConfig,
    working_dir: &Path,
    probe: Probe,
) -> Result<bool, String> {
    let sd = &index.fn_defs[probe.source_fn];
    let sink_cs = &index.call_sites[probe.sink_call];
    let opts = taint::TraceOptions::default();
    taint::trace(
        index,
        config,
        working_dir,
        &working_dir.join(&sd.file),
        sd.line,
        &working_dir.join(&sink_cs.file),
        sink_cs.line,
        &opts,
    )
    .map(|res| !res.paths.is_empty())
    .map_err(|e| format!("{e}"))
}

fn pick_req_to_sink(index: &taint::SymbolIndex) -> Option<Probe> {
    const HINTS: &[&str] = &["req", "request", "input", "body", "query", "user"];
    const SINKS: &[&str] = &["execute", "exec", "query", "system", "spawn", "eval", "write", "run"];
    let source_fn = find_def_with_param(index, HINTS)?;
    let sink_call = find_call_to(index, SINKS, source_fn)?;
    Some(Probe { source_fn, sink_call })
}

fn pick_config_to_write(index: &taint::SymbolIndex) -> Option<Probe> {
    const HINTS: &[&str] = &["config", "cfg", "opts", "options", "settings", "path", "file"];
    const SINKS: &[&str] = &["write", "save", "store", "send", "push"];
    let source_fn = find_def_with_param(index, HINTS)?;
    let sink_call = find_call_to(index, SINKS, source_fn)?;
    Some(Probe { source_fn, sink_call })
}

fn pick_first_to_first(index: &taint::SymbolIndex) -> Option<Probe> {
    let source_fn = index
        .fn_defs
        .iter()
        .position(|d| !d.params.is_empty())?;
    let sink_call = index
        .call_sites
        .iter()
        .position(|cs| cs.in_fn != source_fn && !cs.callee.is_empty())?;
    Some(Probe { source_fn, sink_call })
}

fn find_def_with_param(index: &taint::SymbolIndex, hints: &[&str]) -> Option<usize> {
    for (id, def) in index.fn_defs.iter().enumerate() {
        if def
            .params
            .iter()
            .any(|p| hints.iter().any(|h| p.eq_ignore_ascii_case(h)))
        {
            return Some(id);
        }
    }
    None
}

fn find_call_to(index: &taint::SymbolIndex, hints: &[&str], skip_fn: usize) -> Option<usize> {
    index.call_sites.iter().position(|cs| {
        cs.in_fn != skip_fn && hints.iter().any(|h| cs.callee.eq_ignore_ascii_case(h))
    })
}
