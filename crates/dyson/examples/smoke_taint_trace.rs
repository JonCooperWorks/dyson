// Smoke-test taint_trace against local repos.  Finds bugs the isolated
// unit/integration tests miss by running the full pipeline on real code
// at scale.  Each bug found here should graduate to a deterministic test
// in `tests/ast_taint_patterns.rs`.
//
// Run with:
//     cargo run -p dyson --example smoke_taint_trace --release

use std::path::PathBuf;
use std::time::Instant;

use dyson::ast::{self, taint};

struct RepoTarget {
    label: String,
    path: PathBuf,
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
    let home = std::env::var("HOME").expect("HOME must be set");
    let dev = PathBuf::from(&home).join("Development");
    let smoke_repos = PathBuf::from("/tmp/smoke_repos");

    // (base, repo, optional_subpath, language).  base 'd' = ~/Development,
    // base 's' = /tmp/smoke_repos.  Subpath empty = whole repo.
    let targets_spec: &[(&str, &str, &str, &str)] = &[
        // base 'd' = ~/Development, base 's' = /tmp/smoke_repos
        // Rust
        ("d", "dyson", "", "rust"),
        ("d", "dyson", "crates/dyson/src", "rust"),
        ("d", "dyson", "crates/swarm/src", "rust"),
        ("d", "casino", "", "rust"),
        ("d", "graviola", "", "rust"),
        ("d", "rLLM", "", "rust"),
        ("d", "jotcrack", "", "rust"),
        ("d", "gpuprogramming", "", "rust"),
        // Go
        ("d", "noroot", "", "go"),
        ("d", "harness", "", "go"),
        ("d", "gpu", "", "go"),
        ("d", "arena.targetpractice.network", "", "go"),
        // Python
        ("d", "targetpractice.network", "", "python"),
        ("d", "llama-cpp-turboquant", "", "python"),
        // TypeScript
        ("d", "shannon", "", "typescript"),
        ("d", "familyoffice", "", "typescript"),
        ("d", "familyoffice", "backend/src", "typescript"),
        ("d", "targetpractice.network", "frontend/src", "typescript"),
        // Swift
        ("d", "MetalLearning", "", "swift"),
        ("d", "blackhole", "", "swift"),
        // Zig
        ("d", "nullclaw", "", "zig"),
        // Kotlin
        ("d", "llama-cpp-turboquant", "examples/llama.android", "kotlin"),
        // C#
        ("d", "Terra", "", "csharp"),
        // C++
        ("d", "llama-cpp-turboquant", "", "cpp"),
        // ---- /tmp/smoke_repos — external repos for missing-language coverage ----
        ("s", "spring-petclinic", "", "java"),
        ("s", "sinatra", "", "ruby"),
        ("s", "cJSON", "", "c"),
        ("s", "plug", "", "elixir"),
        ("s", "cowboy", "", "erlang"),
        ("s", "pandoc", "", "haskell"),
        ("s", "dune", "", "ocaml"),
        ("s", "samples", "", "csharp"),
        ("s", "nixpkgs", "pkgs/top-level", "nix"),
    ];

    let targets: Vec<RepoTarget> = targets_spec
        .iter()
        .filter_map(|(base, name, sub, language)| {
            let root = match *base {
                "d" => &dev,
                "s" => &smoke_repos,
                _ => return None,
            };
            let path = if sub.is_empty() {
                root.join(name)
            } else {
                root.join(name).join(sub)
            };
            if !path.exists() {
                return None;
            }
            let label = if sub.is_empty() {
                (*name).to_string()
            } else {
                format!("{name}/{sub}")
            };
            Some(RepoTarget {
                label,
                path,
                language,
            })
        })
        .collect();

    let mut totals = Totals::default();

    for t in &targets {
        println!("\n═══ {} [{}] @ {} ═══", t.label, t.language, t.path.display());
        match run_repo(t, &mut totals).await {
            Ok(()) => {}
            Err(e) => {
                totals.errors += 1;
                eprintln!("  ERROR: {e}");
            }
        }
    }

    println!(
        "\n═══ summary ═══\nrepos: {} | trace-errors: {} | traces-attempted: {} | traces-with-path: {} | index-build-errors: {}",
        targets.len(),
        totals.errors,
        totals.traces_attempted,
        totals.traces_with_path,
        totals.index_errors,
    );
    if totals.errors > 0 || totals.index_errors > 0 {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Totals {
    errors: usize,
    traces_attempted: usize,
    traces_with_path: usize,
    index_errors: usize,
}

async fn run_repo(t: &RepoTarget, totals: &mut Totals) -> Result<(), String> {
    let config = ast::config_for_language_name(t.language)
        .ok_or_else(|| format!("unknown language: {}", t.language))?;

    let started = Instant::now();
    let index = match taint::build_index(config, &t.path).await {
        Ok(i) => i,
        Err(e) => {
            totals.index_errors += 1;
            return Err(format!("build_index: {e}"));
        }
    };
    let elapsed = started.elapsed();

    let unresolved_ratio = if index.call_sites.is_empty() {
        0.0
    } else {
        index.unresolved_callees as f32 / index.call_sites.len() as f32
    };

    println!(
        "  index: {} files, {} defs, {} calls, {} assigns, {} unresolved ({:.0}%), build {:.2}s{}",
        index.file_mtimes.len(),
        index.fn_defs.len(),
        index.call_sites.len(),
        index.assignments.len(),
        index.unresolved_callees,
        unresolved_ratio * 100.0,
        elapsed.as_secs_f32(),
        if index.truncated { " [TRUNCATED]" } else { "" },
    );

    // Try MULTIPLE source/sink pairs per repo to stress different shapes.
    // Each "probe" exercises the full trace pipeline on real code.
    for (label, picker) in &[
        ("req→sink", pick_probe_req_to_sink as PickFn),
        ("config→write", pick_probe_config_to_write),
        ("first-fn→first-call", pick_probe_first_to_first),
    ] {
        if let Some(probe) = picker(&index) {
            totals.traces_attempted += 1;
            let outcome = run_probe(&index, config, &t.path, probe).await;
            match outcome {
                Ok(true) => {
                    totals.traces_with_path += 1;
                    println!("  {label}: PATH");
                }
                Ok(false) => {
                    println!("  {label}: no-path");
                }
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
    tainted_param: String,
}

async fn run_probe(
    index: &taint::SymbolIndex,
    config: &'static ast::LanguageConfig,
    working_dir: &std::path::Path,
    probe: Probe,
) -> Result<bool, String> {
    let sd = &index.fn_defs[probe.source_fn];
    let sink_cs = &index.call_sites[probe.sink_call];
    let source_abs = working_dir.join(&sd.file);
    let sink_abs = working_dir.join(&sink_cs.file);
    let opts = taint::TraceOptions::default();
    match taint::trace(
        index,
        config,
        working_dir,
        &source_abs,
        sd.line,
        &sink_abs,
        sink_cs.line,
        &opts,
    ) {
        Ok(res) => Ok(!res.paths.is_empty()),
        Err(e) => Err(format!("{e} (source {}:{} tainted={}, sink {}:{})",
            sd.file.display(), sd.line, probe.tainted_param,
            sink_cs.file.display(), sink_cs.line)),
    }
}

fn pick_probe_req_to_sink(index: &taint::SymbolIndex) -> Option<Probe> {
    const HINTS: &[&str] = &["req", "request", "input", "body", "query", "user"];
    const SINKS: &[&str] = &["execute", "exec", "query", "system", "spawn", "eval", "write", "run"];
    let (source_fn, tainted_param) = find_def_with_param(index, HINTS)?;
    let sink_call = find_call_to(index, SINKS, source_fn)?;
    Some(Probe { source_fn, sink_call, tainted_param })
}

fn pick_probe_config_to_write(index: &taint::SymbolIndex) -> Option<Probe> {
    const HINTS: &[&str] = &["config", "cfg", "opts", "options", "settings", "path", "file"];
    const SINKS: &[&str] = &["write", "save", "store", "send", "push"];
    let (source_fn, tainted_param) = find_def_with_param(index, HINTS)?;
    let sink_call = find_call_to(index, SINKS, source_fn)?;
    Some(Probe { source_fn, sink_call, tainted_param })
}

fn pick_probe_first_to_first(index: &taint::SymbolIndex) -> Option<Probe> {
    // Stress test: first def that has a param → first call that uses it.
    // Likely to NOT find a path, but must not crash or error.
    let (source_fn, def) = index.fn_defs.iter().enumerate().find(|(_, d)| !d.params.is_empty())?;
    let first_param = def.params.first()?.clone();
    let sink_call = index.call_sites.iter().position(|cs| cs.in_fn != source_fn && !cs.callee.is_empty())?;
    Some(Probe { source_fn, sink_call, tainted_param: first_param })
}

fn find_def_with_param(
    index: &taint::SymbolIndex,
    hints: &[&str],
) -> Option<(usize, String)> {
    for (id, def) in index.fn_defs.iter().enumerate() {
        for p in &def.params {
            if hints.iter().any(|h| p.eq_ignore_ascii_case(h)) {
                return Some((id, p.clone()));
            }
        }
    }
    None
}

fn find_call_to(
    index: &taint::SymbolIndex,
    hints: &[&str],
    skip_fn: usize,
) -> Option<usize> {
    for (i, cs) in index.call_sites.iter().enumerate() {
        if cs.in_fn == skip_fn {
            continue;
        }
        if hints.iter().any(|h| cs.callee.eq_ignore_ascii_case(h)) {
            return Some(i);
        }
    }
    None
}
