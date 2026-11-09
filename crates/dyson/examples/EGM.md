# dyson examples

## Overview
These are standalone `cargo run --example` binaries for the `dyson` crate. They exercise the AST and security tools against real open-source repositories, and drive or score the `security_engineer` harness against a real LLM. They are not cargo tests. `cargo test -p dyson` compiles the examples but does not run them.

## Interface
- `smoke_ast_describe` ([smoke_ast_describe.rs:main]):
  - Runs `dyson::tool::security::ast_describe::describe_source` on fixed per-language snippets, which must contain the expected node kinds.
  - Then runs it on files from shallow-cloned target repos.
  - Free, but needs network for clones.
- `smoke_ast_query` ([smoke_ast_query.rs:main]): runs `dyson::tool::security::ast_query::execute_query_string` patterns per language against snippets and cloned repos.
- `smoke_taint_trace` ([smoke_taint_trace.rs:run]):
  - Builds indexes with `dyson::ast::taint::build_index`, reports `Confidence::from_unresolved_ratio`, and runs `taint::trace` probes with `TraceOptions::default()` on cloned repos.
- `expensive_live_security_review` (billable) ([expensive_live_security_review.rs:Args], [expensive_live_security_review.rs:run]):
  - Required flag: `--config <dyson.json>`.
  - Optional flags: `--model`, `--target <name>` or `--expensive-scan-all-targets`, `--report-suffix`, `--output-dir` (default `test-output`), `--ref`, `--hints on|off` (default off).
  - Runs the `security_engineer` tool against the fixed `TARGETS` list.
- `security_engineer_stage_smoke` (billable) ([security_engineer_stage_smoke.rs:Args], [security_engineer_stage_smoke.rs:run]):
  - Flags: `--target`, `--stage recon|hunt|validate|gapfill|dedupe|trace|feedback|report`, `--task`, `--context`, `--run-id`, `--config` (default `dyson.json`), `--model`, `--output-dir` (default `stage-smoke-output`).
  - Runs one harness stage and copies the checkpoint.
- `security_engineer_benchmark_score` ([security_engineer_benchmark_score.rs:Args], [security_engineer_benchmark_score.rs:main]):
  - Flags: `--manifest <json>` and `--reports-dir <dir>...`.
  - Scores `<case-id>.json` reports for root-cause recall and forbidden-match rate.
- Environment variables:
  - `RUST_LOG` sets the tracing filter ([expensive_live_security_review.rs:init_tracing], [security_engineer_stage_smoke.rs:init_tracing]).
  - `DYSON_SMOKE_COMPILE_ONLY` makes the AST smoke examples skip the clone and path-mode phase ([smoke_ast_describe.rs:main], [smoke_ast_query.rs:main]).

## Invariants
- `expensive_live_security_review` refuses to run without `--target` or `--expensive-scan-all-targets`. Passing both is an error, and so is an unknown target name ([expensive_live_security_review.rs:run]).
- `security_engineer_stage_smoke` requires `--run-id` for any stage other than `recon`, and errors if `--target` does not exist. Per its doc comment, it exits non-zero when the tool output is an error, when the checkpoint is missing, or when the saved `current_stage` is behind the requested stage ([security_engineer_stage_smoke.rs:run], [security_engineer_stage_smoke.rs:stage_at_or_past]).
- `security_engineer_benchmark_score` ([security_engineer_benchmark_score.rs:main]):
  - Accepts only manifest `schema_version == 1`.
  - Flags empty or duplicate finding ids, and affected paths that lack a `:line` or `:start-end` suffix.
  - Exits 1 on missing reports, invalid citations, recall below `min_root_cause_recall`, or a forbidden rate above `max_forbidden_match_rate`.
- A finding matches an expectation only when all of these hold: same `class_id`, matching path substring, matching root-cause or title substring, and severity at or above the minimum ([security_engineer_benchmark_score.rs:matches_expectation]).
- The smoke examples exit 1 if there are any errors or clone failures. `smoke_taint_trace` also exits 1 on index errors ([smoke_ast_describe.rs:main], [smoke_taint_trace.rs:run]).
- The live examples force a sandbox bypass with `dyson::sandbox::sandbox_bypass_from_cli_flag(true)`, on the stated grounds that they only do read-only review ([expensive_live_security_review.rs:run], [security_engineer_stage_smoke.rs:run]).

## Side Effects
- Process and network: run `git clone --depth 1` (or `init`, `fetch --depth 1 <ref>` and `checkout FETCH_HEAD`) from `https://github.com/{slug}.git` ([expensive_live_security_review.rs:shallow_clone], [smoke_taint_trace.rs:shallow_clone]).
- Disk: a shared clone cache at `$TMPDIR/dyson-smoke-repos` ([smoke_ast_describe.rs:main], [expensive_live_security_review.rs:run]).
- Disk: reports are written to `--output-dir` ([expensive_live_security_review.rs:run_target], [security_engineer_stage_smoke.rs:run]).
- Network and cost: billable LLM calls through the provider configured in `dyson.json`, via `load_settings`, `ClientRegistry::new`, `create_skills` and `sec_eng.run` ([expensive_live_security_review.rs:run], [security_engineer_stage_smoke.rs:run]).

## Verification
- Run: `cargo build -p dyson --examples` (from `dyson/`)
- Offline smoke: `DYSON_SMOKE_COMPILE_ONLY=1 cargo run -p dyson --example smoke_ast_describe --release`
