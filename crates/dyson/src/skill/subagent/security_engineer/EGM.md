# Security Engineer Harness (`skill/subagent/security_engineer`)

## Overview
Staged security-research harness behind the parent-facing `security_engineer` tool. The orchestrator drives Recon -> Hunt -> Validate -> Gapfill/follow-up -> Dedupe -> Trace -> Judgment -> Feedback -> Report, spawning one child agent per LLM-backed stage (and class/stack specialist hunters in parallel waves). Each stage persists a durable JSON checkpoint so runs can resume; a cross-run findings ledger deduplicates findings by stable fingerprint; the final report is rendered to Markdown, with a deterministic fallback when LLM report repair fails.

## Interface
- `run_security_harness(rt: SecurityHarnessRuntime) -> Result<ToolOutput>` (crate) - harness entry [mod.rs:run_security_harness]
- `security_engineer_config() -> OrchestratorConfig`, `harness_stages()`, `resolve_stage_models()` [mod.rs:security_engineer_config, mod.rs:harness_stages, mod.rs:resolve_stage_models]
- Constants: `SECURITY_HARNESS_SCHEMA_VERSION = 1`, `SECURITY_HARNESS_VERSION = "security-harness-v3"`, `SUPPORTED_HARNESS_VERSIONS` (v1..v3), `HUNT_SPECIALIST_BACKSTOP = 64` [mod.rs:SECURITY_HARNESS_VERSION]
- Re-exported data types: `SecurityCheckpoint`, `SecurityFinding`, `SecurityTask`, `SecurityHarnessStage`, `SecurityHarnessReport`, `ValidationDecision`, `ValidationDecisionKind`, `TraceResult`, `JudgmentResult`, `DedupeGroup`, `CoverageGap`, `RunHealth`, `LedgerSummary`, `TaskStatus`, `TargetRef`, `ModelMetadata`, and others [mod.rs, types.rs:SecurityCheckpoint]
- `validate_report_json(...)` [parse.rs:validate_report_json], `dedupe_findings(findings)` [report.rs:dedupe_findings]
- `VulnerabilityClassDefinition`, `vulnerability_taxonomy()` - static class table [taxonomy.rs:vulnerability_taxonomy]
- `SecurityHarnessRuntime`, `spawn_stage`, `merge_stage_tool_output` [runtime.rs:SecurityHarnessRuntime, runtime.rs:spawn_stage]
- Stage runners `run_recon_stage`, `run_hunt_stage`, `run_validate_stage`, `run_gapfill_stage`, `run_dedupe_stage`, `run_trace_stage`, `run_judgment_stage`, `run_feedback_stage`, `run_report_stage`, `resolve_repaired_or_fallback_report` [stages.rs:run_recon_stage]
- `CheckpointStore`, `load_checkpoint_for_resume`, `make_run_id`, `should_stop_after` [checkpoint.rs:CheckpointStore]
- `FindingsLedger`, `LedgerStore`, `finding_fingerprint`, `reconcile_findings_ledger` [ledger.rs:FindingsLedger, ledger.rs:finding_fingerprint]
- `stack_specialists(detection)`, `class_provably_inapplicable`, `prune_inapplicable_class_tasks` [stack.rs:stack_specialists]
- `extract_json`, `parse_stage_json`, `parse_report_output`, `parse_validate_output` [parse.rs:extract_json]
- `render_report_markdown`, `render_run_health`, `report_from_checkpoint`, `save_report_document` [report.rs:render_report_markdown]

## Invariants
- Stage children may use only `DIRECT_TOOLS`: `bash`, `read_file`, `search_files`, `list_files`, `ast_describe`, `ast_query`, `attack_surface_analyzer`, `exploit_builder`, `taint_trace`, `dependency_scan` [mod.rs:DIRECT_TOOLS]
- Iteration caps: Recon 60, Hunt 28, Validate 16, Trace 16, Judgment 16 [mod.rs:RECON_MAX_ITERATIONS]; hunt child timeout `HUNT_MAX_ITERATIONS * 120 s` [stages.rs:HUNT_CHILD_TIMEOUT]
- Hunt concurrency defaults to 6, overridable by `DYSON_SEC_HUNT_CONCURRENCY` clamped to 1..=32 [stages.rs:stage_concurrency]; Validate/Trace shard at `STAGE_SHARD_SIZE = 8` findings [stages.rs:STAGE_SHARD_SIZE]
- Per-stage model overrides: `DYSON_SEC_{RECON,HUNT,VALIDATE,TRACE,JUDGMENT,REPORT}_MODEL` [mod.rs:STAGE_MODEL_ENV]; the cross-model judge question is asked at most once per process [mod.rs:secondary_model_memo]
- The taxonomy backfills a generic hunt task per class so no class is silently skipped [taxonomy.rs:ensure_taxonomy_hunt_tasks]; stack pruning drops only provably moot classes [stack.rs:class_provably_inapplicable]
- `extract_json` returns the LAST brace-balanced candidate that parses [parse.rs:extract_json]; all LLM-facing `Deserialize` fields default so malformed output cannot poison the harness [types.rs]
- Finding fingerprints ignore free-text `title`/`root_cause` phrasing [ledger.rs:finding_fingerprint]; ledger schema version 1 [ledger.rs:LEDGER_SCHEMA_VERSION]
- Suggested patches are rendered for human review only and never applied [report.rs:append_suggested_patch]

## Side Effects
- Writes checkpoints under `kb/security-harness/checkpoints` in the workspace (Swarm-mirrored), else `.dyson/security-harness/` in the working directory [checkpoint.rs:CHECKPOINT_PREFIX, checkpoint.rs:CheckpointStore]
- Reads/writes `kb/security-harness/findings-ledger.json` (with local fallback) [ledger.rs:LEDGER_PATH, ledger.rs:LedgerStore]
- Writes reports under `kb/security-harness/reports` [report.rs:REPORTS_PREFIX, report.rs:save_report_document]
- Spawns child agent loops per stage/specialist [runtime.rs:spawn_stage]; may raise an elicitation to choose a secondary model [mod.rs:elicit_secondary_model]
- Reads git refs of the target [checkpoint.rs:git_ref_for]; reads `DYSON_SEC_*` environment variables [stages.rs:requeue_shallow_enabled]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
