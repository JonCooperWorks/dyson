// Expensive live security-review harness.  Drives the real
// `security_engineer` orchestrator against fixed, deliberately-
// vulnerable codebases and writes the LLM's full report to disk.
//
// **This costs real money.**  It spins up the complete subagent stack
// — direct tools + inner planner/researcher/coder/verifier — and makes
// billable LLM calls against the provider in your `dyson.json`.  Unlike
// the `smoke_*` examples (which exercise tool functions in isolation
// and cost nothing to run), a full sweep here can run tens of thousands
// of tokens per target.  There is no free fallback — the example will
// just error if the API rejects the model or runs out of credits.
//
// Each target is shallow-cloned into `$TMPDIR/dyson-smoke-repos/`
// (shared with the other smoke tests).  Reports land in
// `/tmp/dyson-security-review-<name>.md`.
//
// This is NOT a cargo test.  Run explicitly with:
//     cargo run -p dyson --example expensive_live_security_review \
//         --release -- \
//         --config /path/to/dyson.json \
//         [--model <id>] \
//         (--target <name> | --expensive-scan-all-targets)

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use clap::Parser;
use serde_json::json;

use dyson::config::loader::load_settings;
use dyson::controller::ClientRegistry;
use dyson::sandbox::create_sandbox;
use dyson::skill::create_skills;
use dyson::tool::{Tool, ToolContext};

#[derive(Parser)]
#[command(about = "Run the Dyson security_engineer against fixed vulnerable targets.")]
struct Args {
    /// Path to dyson.json config file.  Providers, API keys, and rate
    /// limits all come from here — the example overrides nothing it
    /// doesn't have to.
    #[arg(long)]
    config: PathBuf,

    /// Optional override for `agent.model`.  By default the example
    /// uses whatever `dyson.json` resolves to (the active provider's
    /// first configured model).  Pass this only when you want to swap
    /// in a different model for a single run without editing the config.
    #[arg(long)]
    model: Option<String>,

    /// Run only one target by short name (e.g. `juice-shop`).
    #[arg(long)]
    target: Option<String>,

    /// Run every entry in `TARGETS`.  The name is deliberately long
    /// because the full sweep is billable — you're shallow-cloning
    /// several vulnerable repos and running a real LLM review against
    /// each.  Mutually exclusive with `--target`.
    #[arg(long)]
    expensive_scan_all_targets: bool,

    /// Optional suffix appended to report filenames
    /// (`/tmp/dyson-security-review-<target>[-<suffix>].md`).  Use this
    /// to keep multiple runs against the same target from overwriting
    /// each other — particularly when measuring run-to-run variance.
    #[arg(long)]
    report_suffix: Option<String>,

    /// Override the git ref (tag, branch, or commit SHA) checked out
    /// for the target.  Takes precedence over `Target::git_ref`.  Use
    /// this to review a specific historical version — particularly for
    /// reproducing a published CVE against the exact vulnerable release.
    /// Example: `--target juice-shop --ref v15.0.0`.  Cache directory
    /// includes the ref so different versions of the same target don't
    /// collide.  Use full 40-character SHAs; GitHub rejects short SHAs
    /// in the upload-pack protocol (`couldn't find remote ref`).
    #[arg(long = "ref")]
    git_ref: Option<String>,

    /// Toggle the security_engineer's language / framework cheatsheet
    /// injection.  Default `on` — the orchestrator detects manifests in
    /// the review root and appends matching sheets (lang/framework)
    /// onto the child agent's system prompt before the first turn.
    /// Pass `off` to disable injection for a run; pairs with
    /// `--report-suffix` so A/B diffs are straightforward.
    ///
    /// Implemented by setting `DYSON_SECURITY_ENGINEER_CHEATSHEETS` in
    /// the example process's environment — `OrchestratorTool` checks
    /// that variable at `run()` time.  Env-gating keeps the example
    /// from having to rebuild the OrchestratorConfig, which is shipped
    /// as an `Arc<dyn Tool>` via `create_skills`.
    #[arg(long, value_enum, default_value_t = CheatsheetMode::On)]
    cheatsheets: CheatsheetMode,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CheatsheetMode {
    On,
    Off,
}

struct Target {
    /// Short name used for `--target` and the report filename.
    name: &'static str,
    /// `org/repo` on github.com.
    slug: &'static str,
    /// Subpath inside the repo to scope the review.  Keeps the attack
    /// surface small enough that the agent can enumerate it within its
    /// iteration budget.
    sub: &'static str,
    /// Human-readable description threaded into the task context.
    description: &'static str,
    /// Optional git ref (tag, branch, or commit SHA) to check out.
    /// `None` = shallow-clone the default branch head (latest).  `Some`
    /// pins to a specific version — useful for reproducing published
    /// CVEs against the exact vulnerable release.  Overridden by the
    /// `--ref` CLI flag when present.
    git_ref: Option<&'static str>,
}

const TARGETS: &[Target] = &[
    Target {
        name: "juice-shop",
        slug: "juice-shop/juice-shop",
        sub: "routes",
        description: "OWASP Juice Shop - deliberately vulnerable Node/Express app",
        git_ref: None,
    },
    Target {
        name: "nodegoat",
        slug: "OWASP/NodeGoat",
        sub: "app",
        description: "OWASP NodeGoat - deliberately vulnerable Node/Express app for OWASP Top 10",
        git_ref: None,
    },
    Target {
        name: "railsgoat",
        slug: "OWASP/railsgoat",
        sub: "app",
        description: "OWASP RailsGoat - deliberately vulnerable Ruby on Rails app",
        git_ref: None,
    },
    Target {
        name: "dyson",
        slug: "joncooperworks/dyson",
        sub: "",
        description: "Rust based agent - review the app for AI and rust vulnerabilities",
        git_ref: None,
    },
    // --- Pinned-version targets for CVE-reproduction runs -----------------
    //
    // These entries pin a specific release so the reviewer can be
    // compared against published advisories.  Keep the sub scoped tight —
    // React's monorepo is too large to review wholesale in the 20-iter
    // budget.  `packages/react-dom/src/server` is the historical CVE
    // hotspot (SSR HTML escape bugs).
    Target {
        name: "react-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-dom/src/server",
        description: "React 19.2.0 - packages/react-dom/src/server (SSR render / HTML escape path) for CVE repro",
        git_ref: Some("v19.2.0"),
    },
    Target {
        name: "react-server-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-server/src",
        description: "React 19.2.0 - packages/react-server/src (Fizz streaming SSR + RSC protocol core - HTML escape logic lives here)",
        git_ref: Some("v19.2.0"),
    },
    Target {
        name: "react-server-dom-webpack-19.2.0",
        slug: "facebook/react",
        sub: "packages/react-server-dom-webpack/src",
        description: "React 19.2.0 - packages/react-server-dom-webpack/src (RSC + Server Actions over Webpack - new attack surface)",
        git_ref: Some("v19.2.0"),
    },
];

/// Task body.  The target path is no longer interpolated — it's passed
/// to the orchestrator via the `path` input, which scopes the child
/// agent's working directory.  All tool calls (including `bash`) now
/// resolve relative paths against that scope, matching how the
/// `coder` subagent works.
const REVIEW_PROMPT: &str = "\
Perform a security review of this codebase.  Focus on server-side \
vulnerabilities: authentication flaws, authorization bypasses, \
injection (SQL/NoSQL/command/XSS), insecure deserialization, unsafe \
file handling, hardcoded secrets, and insecure defaults.  Apply the \
Finding Gate strictly - only report findings with concrete attack \
paths and real impact.  Output a markdown report with one section \
per finding: severity, location (file:line), attack path, and \
recommended fix.";

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    init_tracing();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    match rt.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "dyson=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    // --- Load + minimally override settings ---------------------------------
    let mut settings = load_settings(Some(&args.config))?;
    if let Some(m) = args.model {
        settings.agent.model = m;
    }

    // Propagate the --cheatsheets flag into the env so the orchestrator
    // picks it up.  Must happen BEFORE any OrchestratorTool runs.
    match args.cheatsheets {
        CheatsheetMode::On => {
            // SAFETY: single-threaded startup; no concurrent env reads.
            // The reqwest/tokio machinery hasn't spun up background
            // threads that read env yet.
            unsafe { std::env::set_var("DYSON_SECURITY_ENGINEER_CHEATSHEETS", "on") };
        }
        CheatsheetMode::Off => {
            unsafe { std::env::set_var("DYSON_SECURITY_ENGINEER_CHEATSHEETS", "off") };
        }
    }
    println!("cheatsheets: {:?}", args.cheatsheets);
    // The example clones read-only source trees into $TMPDIR and the
    // security_engineer only needs read + ast access.  Skip the macOS
    // container sandbox check — if the user wanted it enforced they'd
    // be driving dyson proper, not an example.
    settings.dangerous_no_sandbox = true;

    // --- Build the same machinery `build_agent` uses ------------------------
    let sandbox = create_sandbox(&settings.sandbox, true);
    let registry = ClientRegistry::new(&settings, None);
    let skills = create_skills(&settings, None, Arc::clone(&sandbox), None, &registry).await;

    // security_engineer is an OrchestratorTool registered inside the
    // SubagentSkill — flatten all skills' tools and find it by name.
    let sec_eng = skills
        .iter()
        .flat_map(|s| s.tools().iter().cloned())
        .find(|t| t.name() == "security_engineer")
        .ok_or("security_engineer tool not registered - check dyson.json `skills`")?;

    println!(
        "using provider={:?} model={}",
        settings.agent.provider, settings.agent.model
    );

    // --- Target cache (shared with smoke tests) -----------------------------
    let cache = std::env::temp_dir().join("dyson-smoke-repos");
    std::fs::create_dir_all(&cache)?;

    // Pick the run list.  `--target X` → just X.  `--expensive-scan-all-targets`
    // → everything.  Neither → fail with a hint; we don't want an
    // accidental invocation to silently fan out across billable runs.
    let selected: Vec<&Target> = match (args.target.as_deref(), args.expensive_scan_all_targets) {
        (Some(_), true) => {
            return Err(
                "--target and --expensive-scan-all-targets are mutually exclusive".into(),
            );
        }
        (Some(name), false) => {
            let matched: Vec<&Target> = TARGETS.iter().filter(|t| t.name == name).collect();
            if matched.is_empty() {
                let known: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
                return Err(format!(
                    "unknown target {name:?}; known: {known:?}"
                )
                .into());
            }
            matched
        }
        (None, true) => TARGETS.iter().collect(),
        (None, false) => {
            let known: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
            return Err(format!(
                "specify either --target <name> or --expensive-scan-all-targets. \
                 known targets: {known:?}"
            )
            .into());
        }
    };

    let suffix = args.report_suffix.as_deref();
    let ref_override = args.git_ref.as_deref();
    for t in selected {
        run_target(t, &cache, &sec_eng, suffix, ref_override).await?;
    }
    Ok(())
}

async fn run_target(
    t: &Target,
    cache: &Path,
    sec_eng: &Arc<dyn Tool>,
    report_suffix: Option<&str>,
    ref_override: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the effective ref: CLI flag wins, else baked-in `git_ref`,
    // else None (clone default branch head).
    let effective_ref: Option<&str> = ref_override.or(t.git_ref);

    // Cache directory includes the ref so `juice-shop@v15.0.0` and
    // `juice-shop@HEAD` don't share a checkout.  Slashes and other
    // filesystem-unfriendly chars in refs get replaced.
    let cache_key = match effective_ref {
        Some(r) => format!(
            "{}__{}",
            t.slug.replace('/', "__"),
            sanitize_ref_for_path(r)
        ),
        None => t.slug.replace('/', "__"),
    };
    let repo_dir = cache.join(&cache_key);
    if !repo_dir.exists() {
        match effective_ref {
            Some(r) => println!("-> cloning {} @ {} ...", t.slug, r),
            None => println!("-> cloning {} ...", t.slug),
        }
        shallow_clone(t.slug, &repo_dir, effective_ref)?;
    }
    let review_root = repo_dir.join(t.sub);
    if !review_root.exists() {
        return Err(format!("subpath missing: {}", review_root.display()).into());
    }
    // Canonicalize so the `path` we hand to the orchestrator is clean —
    // no `..` segments, no symlink wobble between parent and child.
    let review_root = review_root.canonicalize()?;

    match effective_ref {
        Some(r) => println!(
            "\n=== {} [{}] @ {} @ {} ===",
            t.slug,
            t.name,
            r,
            review_root.display()
        ),
        None => println!(
            "\n=== {} [{}] @ {} ===",
            t.slug,
            t.name,
            review_root.display()
        ),
    }

    // ToolContext's working_dir is irrelevant now — the orchestrator's
    // `path` input overrides it for the child.
    let mut ctx = ToolContext::from_cwd()?;
    ctx.dangerous_no_sandbox = true;

    // Fold the version/ref into the context string so the reviewer knows
    // which release it's looking at — relevant when reproducing a CVE
    // against a specific version.
    let context = match effective_ref {
        Some(r) => format!(
            "Target: {} (pinned to {}).\nReview scope: `{}` subpath of {} at {}.",
            t.description, r, t.sub, t.slug, r
        ),
        None => format!(
            "Target: {}.\nReview scope: `{}` subpath of {}.",
            t.description, t.sub, t.slug
        ),
    };

    let input = json!({
        "task": REVIEW_PROMPT,
        "context": context,
        "path": review_root.display().to_string(),
    });

    let started = std::time::Instant::now();
    let output = sec_eng.run(&input, &ctx).await?;
    let elapsed = started.elapsed();

    let filename = match report_suffix {
        Some(s) => format!("dyson-security-review-{}-{s}.md", t.name),
        None => format!("dyson-security-review-{}.md", t.name),
    };
    let report_path = PathBuf::from("/tmp").join(filename);
    std::fs::write(&report_path, &output.content)?;

    println!(
        "  finished in {:.1}s | {} bytes | report -> {}{}",
        elapsed.as_secs_f32(),
        output.content.len(),
        report_path.display(),
        if output.is_error { " [TOOL ERROR]" } else { "" },
    );
    Ok(())
}

fn shallow_clone(slug: &str, dest: &Path, git_ref: Option<&str>) -> Result<(), String> {
    let url = format!("https://github.com/{slug}.git");
    match git_ref {
        None => {
            // Default path: one-shot shallow clone of the default branch.
            let status = Command::new("git")
                .args(["clone", "--depth", "1", "--quiet", &url])
                .arg(dest)
                .status()
                .map_err(|e| format!("spawn git: {e}"))?;
            if !status.success() {
                return Err(format!("git clone {url} exited {status}"));
            }
        }
        Some(r) => {
            // Pinned ref: init + fetch the specific ref + checkout
            // FETCH_HEAD.  Works for tags, branches, and commit SHAs —
            // GitHub allows fetching arbitrary reachable SHAs over the
            // smart HTTP protocol.
            std::fs::create_dir_all(dest)
                .map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
            run_git_in(&["init", "--quiet"], dest)?;
            run_git_in(&["remote", "add", "origin", &url], dest)?;
            run_git_in(&["fetch", "--depth", "1", "--quiet", "origin", r], dest)?;
            run_git_in(&["checkout", "--quiet", "FETCH_HEAD"], dest)?;
        }
    }
    Ok(())
}

fn run_git_in(args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("spawn git {args:?}: {e}"))?;
    if !status.success() {
        return Err(format!(
            "git {args:?} in {} exited {status}",
            cwd.display()
        ));
    }
    Ok(())
}

/// Replace path-unfriendly characters in a git ref so it can safely be
/// a directory-name component.  Slashes become underscores (e.g.
/// `release/v15` → `release_v15`).  Tags and SHAs pass through unchanged.
fn sanitize_ref_for_path(r: &str) -> String {
    r.chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .collect()
}
