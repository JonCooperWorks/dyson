//! Drive the `security_engineer` harness one stage at a time against a real
//! LLM, using the durable checkpoint as the seam between stages.
//!
//! The harness already supports `stop_after_stage` + `resume` + `run_id` as
//! first-class inputs.  This example wires those into a small CLI so you can
//! iterate on prompts and model behavior stage-by-stage instead of paying for
//! a full end-to-end run every time.
//!
//! ## Cost
//!
//! Recon and Hunt are the expensive stages — they spawn child agent loops
//! with real tool calls.  Validate/Trace/Report decide on a bounded findings
//! list and finish in a few iterations.  Order of magnitude (deepseek-v4-pro
//! on a ~100-file scope):
//!
//! | Stage    | Typical cost | Why                                                         |
//! |----------|--------------|-------------------------------------------------------------|
//! | recon    | $1–$3        | Reads the whole scope before emitting tasks                 |
//! | hunt     | $3–$8        | One specialist per taxonomy class, in parallel waves        |
//! | validate | $0.20–$0.80  | One decision per finding                                    |
//! | trace    | $0.20–$0.80  | One reachability decision per finding                       |
//! | report   | $0.10–$0.30  | One structured JSON emit                                    |
//!
//! ## Typical workflow
//!
//! ```bash
//! # 1. Fresh recon (writes a new checkpoint)
//! cargo run --release --example security_engineer_stage_smoke -- \
//!     --config dyson.json \
//!     --target /var/lib/dyson/workspace/programs/vllm \
//!     --task "review vllm distributed/" \
//!     --context "scope: vllm/distributed/ only" \
//!     --stage recon
//! # ↳ prints `run_id=sec-...`; saves checkpoint under kb/security-harness/...
//! #   and a copy of the tool output at ./stage-smoke-output/recon-<run_id>.md
//!
//! # 2. Run just hunt against the recon's checkpoint
//! cargo run --release --example security_engineer_stage_smoke -- \
//!     --target /var/lib/dyson/workspace/programs/vllm \
//!     --task "review vllm distributed/" \
//!     --stage hunt \
//!     --run-id sec-1234567890-abc
//!
//! # 3. Then validate, then trace, then report — each reads the same checkpoint.
//! ```
//!
//! Use `--model` to A/B different models on the same stage cheaply:
//! `--model claude-haiku` vs `--model deepseek/deepseek-v4-pro` against the
//! same recon checkpoint isolates "is this stage's prompt working on this
//! model" from "is my recon any good."
//!
//! ## Artifacts + failure detection
//!
//! Each invocation persists two files under `--output-dir` (default
//! `stage-smoke-output/`):
//!
//! - `<stage>-<run_id>.md`              — the raw tool output (rendered
//!                                         Markdown if it's the report stage)
//! - `<stage>-<run_id>-checkpoint.json` — a copy of the durable
//!                                         `SecurityCheckpoint`, copied
//!                                         from the harness's canonical
//!                                         location `.dyson/security-harness/
//!                                         checkpoints/<run_id>.json`
//!
//! The checkpoint copy is the audit trail.  You can post-mortem a run
//! without grepping the workspace, and the copy survives a `cargo clean`
//! or `.dyson` cleanup.
//!
//! Failure detection is strict — three signals fail the run with a
//! non-zero exit code, so a wrapper script driving the full pipeline
//! under `set -e` stops at the first real regression:
//!
//! 1. `output.is_error == true`              — the tool returned an error
//! 2. The canonical checkpoint file is missing — harness returned success
//!                                                 but didn't write a
//!                                                 checkpoint (silent fail)
//! 3. `current_stage` in the saved checkpoint is BEFORE the stage we asked
//!    for — stage didn't advance (e.g. recon parse failed AND the
//!    non-fatal fallback didn't update `current_stage`)
//!
//! Drive multiple stages from a wrapper:
//!
//! ```bash
//! set -e
//! out=$(cargo run --release --example security_engineer_stage_smoke -- \
//!         --target /path --task "review distributed/" --stage recon)
//! run_id=$(echo "$out" | grep -oE 'run_id=sec-[a-z0-9-]+' | head -1 | cut -d= -f2)
//! for stage in hunt validate gapfill dedupe trace feedback report; do
//!     cargo run --release --example security_engineer_stage_smoke -- \
//!         --target /path --stage "$stage" --run-id "$run_id"
//! done
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use serde_json::Value;

use dyson::config::loader::load_settings;
use dyson::controller::ClientRegistry;
use dyson::sandbox::create_sandbox;
use dyson::skill::create_skills;
use dyson::tool::ToolContext;

#[derive(Parser, Debug)]
#[command(
    about = "Drive the security_engineer harness one stage at a time against a real LLM.",
    long_about = "Each stage reads + writes the same SecurityCheckpoint JSON, so you can iterate \
        on recon, then run hunt against that recon's checkpoint, etc. Costs ~$0.20–$8 per probe \
        depending on stage and model."
)]
struct Args {
    /// Path to the scoped review root the harness will operate on.
    #[arg(long)]
    target: PathBuf,

    /// Stage to run.  For non-recon stages, `--run-id` is required.
    #[arg(long, value_enum)]
    stage: Stage,

    /// Task prompt the parent describes to the orchestrator.  Required for a fresh
    /// recon; used as a label on resumed stages so the checkpoint's user_message
    /// makes sense in the prompt.
    #[arg(long, default_value = "")]
    task: String,

    /// Optional extra context appended to the parent prompt.
    #[arg(long)]
    context: Option<String>,

    /// Existing run_id to resume.  Required for every stage except `recon`.  On a
    /// fresh recon run, the new run_id is printed to stdout and to the saved
    /// output file's first line.
    #[arg(long)]
    run_id: Option<String>,

    /// Path to `dyson.json` (or any settings file load_settings accepts).
    #[arg(long, default_value = "dyson.json")]
    config: PathBuf,

    /// Override the configured provider/model for this run.
    #[arg(long)]
    model: Option<String>,

    /// Directory where the per-stage tool output Markdown is saved.
    #[arg(long, default_value = "stage-smoke-output")]
    output_dir: PathBuf,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Stage {
    Recon,
    Hunt,
    Validate,
    Gapfill,
    Dedupe,
    Trace,
    Feedback,
    Report,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Recon => "recon",
            Stage::Hunt => "hunt",
            Stage::Validate => "validate",
            Stage::Gapfill => "gapfill",
            Stage::Dedupe => "dedupe",
            Stage::Trace => "trace",
            Stage::Feedback => "feedback",
            Stage::Report => "report",
        }
    }

    fn is_fresh(self) -> bool {
        matches!(self, Stage::Recon)
    }
}

fn init_tracing() {
    // Default to INFO — the harness emits a `tracing::warn!` when recon parse
    // falls through, which is exactly the kind of event you want surfaced in a
    // smoke test.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_tracing();
    let args = Args::parse();

    match run(args).await {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if !args.stage.is_fresh() && args.run_id.is_none() {
        return Err(format!(
            "stage `{}` resumes a checkpoint; --run-id is required",
            args.stage.as_str()
        )
        .into());
    }

    if !args.target.exists() {
        return Err(format!("target path does not exist: {}", args.target.display()).into());
    }
    let target = args.target.canonicalize()?;

    let mut settings = load_settings(Some(args.config.as_path()))
        .map_err(|e| format!("load_settings({}): {}", args.config.display(), e))?;

    // Same bypass shape the expensive_live_security_review example uses —
    // read-only review on the host filesystem, no need for the OS sandbox.
    let bypass = dyson::sandbox::sandbox_bypass_from_cli_flag(true);
    settings.sandbox_bypass = bypass.clone();

    if let Some(model) = args.model.as_deref() {
        settings.agent.model = model.to_string();
    }

    let sandbox = create_sandbox(&settings.sandbox, bypass);
    let registry = ClientRegistry::new(&settings, None);
    let skills = create_skills(&settings, None, Arc::clone(&sandbox), None, &registry).await;

    let sec_eng = skills
        .iter()
        .flat_map(|s| s.tools().iter().cloned())
        .find(|t| t.name() == "security_engineer")
        .ok_or("security_engineer tool not registered — check dyson.json `skills`")?;

    let mut ctx = ToolContext::from_cwd()?;
    ctx.sandbox_bypass = dyson::sandbox::sandbox_bypass_from_cli_flag(true);

    let input = build_input(&args, &target);

    println!("=== security_engineer stage smoke ===");
    println!("stage:    {}", args.stage.as_str());
    println!("target:   {}", target.display());
    println!(
        "provider: {:?} model={}",
        settings.agent.provider, settings.agent.model
    );
    if let Some(rid) = &args.run_id {
        println!("resume:   run_id={rid}");
    } else {
        println!("resume:   no (fresh recon)");
    }
    println!();

    let started = std::time::Instant::now();
    let output = sec_eng.run(&input, &ctx).await?;
    let elapsed = started.elapsed();

    std::fs::create_dir_all(&args.output_dir)?;
    let resolved_run_id = extract_run_id(&output.content)
        .or_else(|| args.run_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Persist the tool output Markdown next to the checkpoint copy so a
    // post-mortem has both human-readable and machine-readable artifacts
    // in one place.  The harness writes the canonical checkpoint into the
    // workspace fallback path (`.dyson/security-harness/checkpoints/`);
    // the smoke example copies it out so per-run artifacts live under
    // `--output-dir` and don't get accidentally cleaned by a future
    // `.dyson` cleanup.
    let outfile = args
        .output_dir
        .join(format!("{}-{}.md", args.stage.as_str(), resolved_run_id));
    let header = format!(
        "<!-- stage={} run_id={} target={} model={} elapsed={:.1}s is_error={} -->\n\n",
        args.stage.as_str(),
        resolved_run_id,
        target.display(),
        settings.agent.model,
        elapsed.as_secs_f32(),
        output.is_error,
    );
    std::fs::write(
        &outfile,
        [header.as_bytes(), output.content.as_bytes()].concat(),
    )?;

    // Locate the canonical checkpoint, copy it next to the .md output.
    // From here on the checkpoint is treated as the source of truth — the
    // is_error flag tells us the harness returned an error, but a stage
    // can also fail "softly" (model emitted prose, parse failed but
    // recon-non-fatal kicked in, current_stage didn't advance).  Reading
    // the saved checkpoint gives us a deterministic verdict.
    let canonical_checkpoint = canonical_checkpoint_path(&ctx.working_dir, &resolved_run_id);
    let persisted_checkpoint = args.output_dir.join(format!(
        "{}-{}-checkpoint.json",
        args.stage.as_str(),
        resolved_run_id
    ));
    let mut persisted = false;
    let mut saved_stage: Option<String> = None;
    let mut saved_completed = false;
    if canonical_checkpoint.exists() {
        std::fs::copy(&canonical_checkpoint, &persisted_checkpoint).map_err(|e| {
            format!(
                "copy checkpoint {} -> {}: {e}",
                canonical_checkpoint.display(),
                persisted_checkpoint.display()
            )
        })?;
        persisted = true;
        if let Ok(body) = std::fs::read_to_string(&persisted_checkpoint) {
            if let Ok(json) = serde_json::from_str::<Value>(&body) {
                saved_stage = json
                    .get("current_stage")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                saved_completed = json
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
        }
    }

    println!(
        "{} stage `{}` in {:.1}s | {} bytes | {} artefacts | run_id={} | output -> {}",
        if output.is_error { "✗" } else { "✓" },
        args.stage.as_str(),
        elapsed.as_secs_f32(),
        output.content.len(),
        output.artefacts.len(),
        resolved_run_id,
        outfile.display(),
    );
    if persisted {
        println!("checkpoint -> {}", persisted_checkpoint.display());
        if let Some(stage) = &saved_stage {
            println!("saved checkpoint: current_stage={stage} completed={saved_completed}");
        }
    } else {
        println!(
            "checkpoint NOT persisted — canonical path missing: {}",
            canonical_checkpoint.display()
        );
    }

    if let Some(meta) = output.metadata.as_ref() {
        let in_tok = meta
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let out_tok = meta
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let calls = meta.get("llm_calls").and_then(Value::as_u64).unwrap_or(0);
        println!("tokens:  input={in_tok} output={out_tok} llm_calls={calls}");
    }

    // Failure detection — strict, fail loudly so a wrapper script driving
    // multiple stages with `set -e` can stop at the first regression.
    if output.is_error {
        return Err(format!("stage `{}` returned a tool error", args.stage.as_str()).into());
    }
    if !persisted {
        return Err(format!(
            "stage `{}` did not produce a checkpoint at {} — the harness returned \
             success but no checkpoint was written; treating as a silent failure",
            args.stage.as_str(),
            canonical_checkpoint.display()
        )
        .into());
    }
    if let Some(stage) = saved_stage.as_deref() {
        if !stage_at_or_past(args.stage, stage) {
            return Err(format!(
                "stage `{}` ran but saved checkpoint's current_stage is `{}` — the \
                 stage did not advance, treating as a silent failure",
                args.stage.as_str(),
                stage
            )
            .into());
        }
    }

    // Hint the user about the next command.  The next stage in the canonical
    // order is the one after `args.stage`, so they don't have to look it up.
    if let Some(next) = next_stage(args.stage) {
        println!(
            "\nnext: cargo run --release --example security_engineer_stage_smoke -- \\\n\
             \t--target {} --stage {} --run-id {}",
            target.display(),
            next.as_str(),
            resolved_run_id,
        );
    } else {
        println!("\nfinal stage complete. checkpoint marks completed=true.");
    }

    Ok(())
}

fn build_input(args: &Args, target: &Path) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("task".into(), Value::String(args.task.clone()));
    if let Some(ctx) = &args.context {
        obj.insert("context".into(), Value::String(ctx.clone()));
    }
    obj.insert("path".into(), Value::String(target.display().to_string()));
    obj.insert(
        "stop_after_stage".into(),
        Value::String(args.stage.as_str().into()),
    );
    if let Some(rid) = &args.run_id {
        obj.insert("resume".into(), Value::Bool(true));
        obj.insert("run_id".into(), Value::String(rid.clone()));
    }
    Value::Object(obj)
}

/// Pull the new run_id out of the harness's stop-after content.  It says
/// "security_engineer checkpoint saved after recon. run_id=sec-... path=..."
/// — when present, anchor on the `run_id=` token.
fn extract_run_id(content: &str) -> Option<String> {
    let needle = "run_id=";
    let start = content.find(needle)? + needle.len();
    let tail = &content[start..];
    let end = tail
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(tail.len());
    if end == 0 {
        return None;
    }
    Some(tail[..end].to_string())
}

fn next_stage(stage: Stage) -> Option<Stage> {
    Some(match stage {
        Stage::Recon => Stage::Hunt,
        Stage::Hunt => Stage::Validate,
        Stage::Validate => Stage::Gapfill,
        Stage::Gapfill => Stage::Dedupe,
        Stage::Dedupe => Stage::Trace,
        Stage::Trace => Stage::Feedback,
        Stage::Feedback => Stage::Report,
        Stage::Report => return None,
    })
}

/// Where the harness puts the checkpoint when no Workspace is wired up
/// (the default for this CLI — `ToolContext::from_cwd()` carries no
/// workspace).  Mirrors `CheckpointStore::fallback_dir` in
/// `security_engineer/checkpoint.rs`.
fn canonical_checkpoint_path(working_dir: &Path, run_id: &str) -> PathBuf {
    working_dir
        .join(".dyson")
        .join("security-harness")
        .join("checkpoints")
        .join(format!("{run_id}.json"))
}

/// True when the saved checkpoint's `current_stage` is at or after the
/// stage we just asked the harness to run — i.e. the stage actually
/// advanced.  The canonical order is:
/// recon → hunt → validate → gapfill → dedupe → trace → feedback → report.
fn stage_at_or_past(requested: Stage, saved: &str) -> bool {
    fn rank(stage: &str) -> Option<u32> {
        Some(match stage {
            "recon" => 0,
            "hunt" => 1,
            "validate" => 2,
            "gapfill" => 3,
            "dedupe" => 4,
            "trace" => 5,
            "feedback" => 6,
            "report" => 7,
            _ => return None,
        })
    }
    match (rank(requested.as_str()), rank(saved)) {
        (Some(r), Some(s)) => s >= r,
        _ => true, // unknown saved stage — don't false-positive on an enum mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_run_id_picks_up_the_token() {
        let raw = "security_engineer checkpoint saved after recon. \
                   run_id=sec-1780812345-7 path=kb/security-harness/...";
        assert_eq!(extract_run_id(raw).as_deref(), Some("sec-1780812345-7"));
    }

    #[test]
    fn extract_run_id_handles_missing_token() {
        assert_eq!(extract_run_id("checkpoint saved").as_deref(), None);
    }

    #[test]
    fn next_stage_walks_the_canonical_order() {
        assert!(matches!(next_stage(Stage::Recon), Some(Stage::Hunt)));
        assert!(matches!(next_stage(Stage::Report), None));
    }

    #[test]
    fn fresh_stage_is_only_recon() {
        assert!(Stage::Recon.is_fresh());
        assert!(!Stage::Hunt.is_fresh());
        assert!(!Stage::Report.is_fresh());
    }

    #[test]
    fn canonical_checkpoint_path_lands_under_dot_dyson() {
        let p = canonical_checkpoint_path(Path::new("/tmp/work"), "sec-1-2");
        assert_eq!(
            p,
            PathBuf::from("/tmp/work/.dyson/security-harness/checkpoints/sec-1-2.json")
        );
    }

    #[test]
    fn stage_at_or_past_accepts_same_or_later() {
        // recon ran, saved stage is hunt — the harness already advanced
        assert!(stage_at_or_past(Stage::Recon, "hunt"));
        // recon ran, saved stage is recon — exactly at the requested stage
        assert!(stage_at_or_past(Stage::Recon, "recon"));
        // hunt was requested but the saved stage is still recon — soft failure
        assert!(!stage_at_or_past(Stage::Hunt, "recon"));
        // report ran, saved stage is report — exactly at the requested stage
        assert!(stage_at_or_past(Stage::Report, "report"));
    }

    #[test]
    fn stage_at_or_past_does_not_false_positive_on_unknown_saved() {
        // If the checkpoint format added a new stage name we don't recognise,
        // assume the harness knows what it's doing — don't fail the run on
        // an enum mismatch.
        assert!(stage_at_or_past(Stage::Recon, "future_stage_name"));
    }
}
