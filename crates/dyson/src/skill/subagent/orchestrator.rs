// ===========================================================================
// OrchestratorTool — a generic composable subagent orchestrator.
//
// An OrchestratorTool spawns a child agent that gets:
//   - A filtered set of parent tools (the "direct tools")
//   - Inner subagent tools (planner, researcher, coder, verifier)
//   - A custom system prompt that defines its personality
//
// Any orchestrator role can be composed from an `OrchestratorConfig`:
//   - security_engineer: AST-aware vuln scanning + exploit generation
//   - pentester: authorization-scoped active testing
//   - (future): architect, devops_engineer, data_engineer, etc.
//
// Architecture:
//   Parent (depth 0)
//     └─ orchestrator child (depth 1)
//          ├─ [direct tools: filtered from parent by allowlist]
//          └─ [inner subagents: planner, researcher, coder, verifier]
//               └─ inner subagent children (depth 2, no further subagents)
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::message::{Artefact, ArtefactKind};
use crate::sandbox::Sandbox;
use crate::tool::{CheckpointEvent, Tool, ToolContext, ToolExecutionPlan, ToolOutput};
use crate::workspace::WorkspaceHandle;

use super::{ChildSpawn, spawn_child};

/// Configuration that defines an orchestrator's identity and capabilities.
/// Compose any orchestrator role by filling in this struct.
///
/// Uses `&'static str` for fields that are typically `include_str!()` or
/// string literals (system_prompt, injects_protocol, tool names) to avoid
/// unnecessary heap allocations at startup.
#[derive(Clone, Debug)]
pub struct OrchestratorConfig {
    /// Tool name exposed to the parent agent (e.g., "security_engineer").
    pub name: &'static str,
    /// Tool description shown in the parent's tool list.
    pub description: &'static str,
    /// System prompt for the orchestrator's child agent.
    pub system_prompt: &'static str,
    /// Allowlist of parent tool names this orchestrator gets direct access to.
    /// Tools not in this list are filtered out.
    pub direct_tool_names: &'static [&'static str],
    /// Max iterations for the child agent.
    pub max_iterations: usize,
    /// Max tokens per LLM response.
    pub max_tokens: u32,
    /// Optional protocol fragment injected into the parent's system prompt.
    /// Tells the parent when and how to invoke this orchestrator.
    pub injects_protocol: Option<&'static str>,
    /// When set, the child's final text is wrapped as an `Artefact` of
    /// this kind and attached to the returned `ToolOutput` — the HTTP
    /// controller renders it in the Artefacts tab.  The full text is
    /// still returned as `ToolOutput.content` so the parent LLM sees
    /// the report too.
    ///
    /// `None` (the default) keeps the orchestrator chat-only.  The
    /// security engineer opts in; devops/architect/… remain opt-out.
    pub emit_artefact: Option<ArtefactKind>,
    /// Optional first-party harness behavior layered under this tool name.
    pub harness: Option<OrchestratorHarness>,
}

/// Wall-clock budget for a penetration-test orchestrator invocation. Unlike
/// the checkpointed `SecurityResearch` harness — which the runtime may kill and
/// resume — a pentest runs a single un-checkpointed active-testing loop (recon,
/// hypothesis-driven testing, independent validation, cleanup, reporting). The
/// generic 5-minute tool deadline truncated that loop mid-run, dropping rules-
/// of-engagement enforcement to the model's discretion. Thirty minutes covers a
/// bounded active test (e.g. a 30-request cap at ~1 req/s plus multi-stage
/// reasoning) without being effectively unbounded.
const PENTEST_EXECUTION_TIMEOUT_MS: u64 = 1_800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrchestratorHarness {
    SecurityResearch,
    /// Marker for the active-testing input contract. Execution still uses the
    /// generic orchestrator loop; unlike SecurityResearch it has no private
    /// checkpointed stage runner.
    PenetrationTest,
}

/// A composable `Tool` that spawns an orchestrator child agent.
///
/// The child gets direct tools (filtered from parent) plus inner subagent
/// tools (planner, researcher, coder, verifier) for parallel dispatch.
///
/// Input schema: `{ task: string, context?: string }`
pub struct OrchestratorTool {
    config: OrchestratorConfig,
    provider: LlmProvider,
    /// Model identifier inherited from the parent agent's configuration.
    /// Never falls back to a registry default — subagents must bill the
    /// same model the user configured, not a hardcoded Sonnet.
    model: String,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<WorkspaceHandle>,
    /// Parent tools filtered to `config.direct_tool_names`.
    pub(crate) direct_tools: Vec<Arc<dyn Tool>>,
    /// Inner subagent tools (planner, researcher, coder, verifier) that
    /// the child can dispatch at depth 2.
    pub(crate) inner_subagent_tools: Vec<Arc<dyn Tool>>,
}

impl OrchestratorTool {
    /// Create a new OrchestratorTool from a config.
    ///
    /// `parent_tools` is filtered to `config.direct_tool_names`.
    /// `inner_subagent_tools` are pre-built SubagentTool/CoderTool instances
    /// that the child can invoke (they spawn at depth 2).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: OrchestratorConfig,
        provider: LlmProvider,
        model: String,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<WorkspaceHandle>,
        parent_tools: &[Arc<dyn Tool>],
        inner_subagent_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        let direct_tools = parent_tools
            .iter()
            .filter(|t| config.direct_tool_names.contains(&t.name()))
            .cloned()
            .collect();

        Self {
            config,
            provider,
            model,
            client,
            sandbox,
            workspace,
            direct_tools,
            inner_subagent_tools,
        }
    }

    /// Access the underlying config.
    pub fn config(&self) -> &OrchestratorConfig {
        &self.config
    }
}

#[async_trait]
impl Tool for OrchestratorTool {
    fn name(&self) -> &str {
        self.config.name
    }

    fn description(&self) -> &str {
        self.config.description
    }

    /// A pentest orchestrator supervises a long, un-checkpointed active test, so
    /// it needs a wall-clock budget well beyond the generic 5-minute tool
    /// deadline; every other orchestrator keeps the conservative exclusive
    /// default (checkpointed harnesses tolerate being killed and resumed).
    fn execution_plan(&self, _input: &serde_json::Value, _ctx: &ToolContext) -> ToolExecutionPlan {
        match self.config.harness {
            Some(OrchestratorHarness::PenetrationTest) => ToolExecutionPlan {
                timeout_ms: PENTEST_EXECUTION_TIMEOUT_MS,
                ..ToolExecutionPlan::exclusive()
            },
            _ => ToolExecutionPlan::exclusive(),
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        let required = match self.config.harness {
            Some(OrchestratorHarness::SecurityResearch) => Vec::<&str>::new(),
            // The browser elicitation modal collects and directly confirms the
            // active-test boundary before execution. Headless callers can
            // still provide the three fields inline as a fallback.
            Some(OrchestratorHarness::PenetrationTest) => vec!["task"],
            None => vec!["task"],
        };
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for this orchestrator to perform."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context about the codebase, \
                        recent changes, or specific areas of concern."
                },
                "target": {
                    "type": "string",
                    "description": "Optional inline fallback for headless penetration tests. \
                        In the browser, a preflight elicitation asks the operator for the exact \
                        authorized URL, hostname, address, range, or local service."
                },
                "authorization": {
                    "type": "string",
                    "description": "Optional inline fallback for headless penetration tests. \
                        In the browser, the operator directly supplies an explicit ownership or \
                        authorization statement during preflight."
                },
                "rules_of_engagement": {
                    "type": "string",
                    "description": "Optional inline fallback for headless penetration tests. \
                        In the browser, preflight collects allowed techniques, exclusions, rate \
                        and concurrency limits, accounts, stop conditions, and test window."
                },
                "path": {
                    "type": "string",
                    "description": "Optional directory to scope the orchestrator's \
                        child agent to.  When set, the child's working directory is \
                        this path — relative tool paths resolve against it and `bash` \
                        starts here.  Falls back to the parent's working directory \
                        when omitted."
                },
                "resume": {
                    "type": "boolean",
                    "description": "For durable staged harnesses, resume an incomplete \
                        checkpoint instead of starting a new run."
                },
                "run_id": {
                    "type": "string",
                    "description": "Optional durable staged harness run id to resume."
                },
                "stop_after_stage": {
                    "type": "string",
                    "enum": ["recon", "hunt", "validate", "gapfill", "dedupe", "trace", "judgment", "feedback", "report"],
                    "description": "Optional bounded-run control for smoke tests or operator-driven \
                        checkpoint creation. The harness saves the stage checkpoint and returns \
                        before later stages."
                }
            },
            "required": required
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let mut parsed: OrchestratorInput = serde_json::from_value(input.clone())
            .map_err(|e| DysonError::tool(self.config.name, format!("invalid input: {e}")))?;
        let security_resume = matches!(
            self.config.harness,
            Some(OrchestratorHarness::SecurityResearch)
        ) && (parsed.resume
            || parsed.run_id.as_ref().is_some_and(|s| !s.trim().is_empty()));
        if parsed.task.trim().is_empty() {
            if security_resume {
                parsed.task = "resume security review".into();
            } else {
                return Ok(ToolOutput::error("task is required"));
            }
        }
        if self.config.harness == Some(OrchestratorHarness::PenetrationTest) {
            if let Err(message) = complete_pentest_preflight(&mut parsed).await {
                return Ok(ToolOutput::error(message));
            }
            for (name, value) in [
                ("target", parsed.target.as_str()),
                ("authorization", parsed.authorization.as_str()),
                ("rules_of_engagement", parsed.rules_of_engagement.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Ok(ToolOutput::error(format!(
                        "{name} is required for penetration testing"
                    )));
                }
            }
        }

        // Validate + canonicalize the optional scope path before handing it
        // to the child.  `canonicalize` also implicitly checks existence.
        let scoped_dir = if parsed.path.is_empty() {
            None
        } else {
            let requested = std::path::PathBuf::from(&parsed.path);
            let resolved = if requested.is_absolute() {
                requested
            } else {
                ctx.working_dir.join(&requested)
            };
            let canonical = match resolved.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolOutput::error(format!(
                        "path '{}' cannot be resolved: {e}",
                        parsed.path
                    )));
                }
            };
            if !canonical.is_dir() {
                return Ok(ToolOutput::error(format!(
                    "path '{}' is not a directory",
                    parsed.path
                )));
            }
            Some(canonical)
        };

        let user_message = if self.config.harness == Some(OrchestratorHarness::PenetrationTest) {
            let context = if parsed.context.is_empty() {
                String::new()
            } else {
                format!("\n\nContext:\n{}", parsed.context)
            };
            format!(
                "Authorized target:\n{}\n\nAuthorization statement:\n{}\n\nRules of engagement:\n{}\
                 {context}\n\nTask:\n{}",
                parsed.target, parsed.authorization, parsed.rules_of_engagement, parsed.task
            )
        } else if parsed.context.is_empty() {
            parsed.task.clone()
        } else {
            format!("Context:\n{}\n\nTask:\n{}", parsed.context, parsed.task)
        };

        // Combine direct + inner subagent tools without cloning the Vec.
        let mut all_tools =
            Vec::with_capacity(self.direct_tools.len() + self.inner_subagent_tools.len());
        all_tools.extend(self.direct_tools.iter().cloned());
        all_tools.extend(self.inner_subagent_tools.iter().cloned());

        // Phase checkpoints for UX.  Accumulate events here and attach
        // them to the final ToolOutput so Output::checkpoint is called
        // as each phase boundary crosses — gives the HTTP controller's
        // Artefacts-view UI progress signal instead of multi-minute
        // silence.  Only orchestrators with `emit_artefact` set opt in,
        // to avoid spamming checkpoints on every devops / architect
        // run (those are typically fast chat-shaped calls).
        let mut phase_checkpoints: Vec<CheckpointEvent> = Vec::new();
        let emits_progress = self.config.emit_artefact.is_some();
        if emits_progress {
            phase_checkpoints.push(CheckpointEvent {
                message: format!("{}: preparing review", self.config.name),
                progress: Some(0.0),
            });
        }

        // The child's system prompt is the role prompt as-is.  Framework /
        // language references are no longer injected into the shared prompt:
        // the hunt stage detects the stack and spawns a dedicated specialist
        // hunter per framework/language, each briefed with its own reference.
        let system_prompt = self.config.system_prompt.to_string();

        if emits_progress {
            phase_checkpoints.push(CheckpointEvent {
                message: format!(
                    "{}: subagent analysing — this may take several minutes",
                    self.config.name
                ),
                progress: Some(0.1),
            });
        }

        if self.config.harness == Some(OrchestratorHarness::SecurityResearch) {
            return super::security_engineer::run_security_harness(
                super::security_engineer::SecurityHarnessRuntime {
                    config_name: self.config.name,
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    stage_models: super::security_engineer::resolve_stage_models(),
                    client: self.client.clone(),
                    sandbox: Arc::clone(&self.sandbox),
                    workspace: self.workspace.clone(),
                    parent_depth: ctx.depth,
                    scoped_dir,
                    parent_working_dir: ctx.working_dir.clone(),
                    all_tools,
                    system_prompt,
                    user_message,
                    parsed,
                    activity: ctx.activity.clone(),
                    events: ctx.subagent_events.clone(),
                    parent_tool_id: ctx.tool_use_id.clone(),
                    emit_artefact: self.config.emit_artefact,
                    max_tokens: self.config.max_tokens,
                },
            )
            .await;
        }

        let settings = AgentSettings::for_child(
            self.model.clone(),
            self.provider.clone(),
            self.config.max_iterations,
            self.config.max_tokens,
            system_prompt,
        );

        let started_at = std::time::SystemTime::now();
        let started_epoch = unix_seconds(started_at);

        // Activity tab registration — UI-only side channel.  Records a
        // `Running` row for the Subagents lane; finished explicitly
        // below with duration + outcome.  `None` on non-HTTP controllers.
        let mut activity_token = ctx.activity.as_ref().map(|a| {
            a.start(
                crate::controller::LANE_SUBAGENT,
                self.config.name,
                &crate::controller::truncate_note(&user_message, 80),
            )
        });

        let child_working_dir = scoped_dir.clone();
        let spawn_result = spawn_child(ChildSpawn {
            name: self.config.name,
            settings,
            inherited_tools: all_tools,
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth: ctx.depth,
            working_dir: child_working_dir,
            user_message,
            activity: ctx.activity.clone(),
            events: ctx.subagent_events.clone(),
            parent_tool_id: ctx.tool_use_id.clone(),
        })
        .await;
        let mut out = match spawn_result {
            Ok(o) => o,
            Err(e) => {
                if let Some(tok) = activity_token.take() {
                    tok.finish(crate::controller::ActivityStatus::Err, Some("spawn failed"));
                }
                return Err(e);
            }
        };

        // Pre-pend the phase checkpoints we queued BEFORE the child
        // ran, then add a terminal checkpoint reporting the outcome.
        // Outer Vec::splice keeps ordering stable.
        if emits_progress {
            let elapsed = unix_seconds(std::time::SystemTime::now()).saturating_sub(started_epoch);
            let terminal = if out.is_error {
                CheckpointEvent {
                    message: format!("{}: failed after {}s", self.config.name, elapsed),
                    progress: Some(1.0),
                }
            } else {
                CheckpointEvent {
                    message: format!(
                        "{}: completed in {}s · {} bytes",
                        self.config.name,
                        elapsed,
                        out.content.len(),
                    ),
                    progress: Some(1.0),
                }
            };
            phase_checkpoints.push(terminal);
            // Front-load phase checkpoints so they fire before any
            // artefact emission on the controller side.
            let mut merged = phase_checkpoints;
            merged.append(&mut out.checkpoints);
            out.checkpoints = merged;
        }

        // Programmatic artefact emission: ANY successful orchestrator
        // run with non-empty output produces an artefact when the config
        // opts in.  We deliberately do NOT gate on output shape — a
        // prior `looks_like_report` heuristic (>500 chars, leading `#`)
        // caused silent data loss when a weaker model returned an
        // answer that didn't hit those markers, producing a run with a
        // security review in `ToolOutput.content` but no artefact in
        // the UI.  The user-visible contract is: opt-in configs always
        // emit; emptiness or error is the only suppression.
        if let Some(kind) = self.config.emit_artefact
            && !out.is_error
            && !out.content.trim().is_empty()
        {
            let finished_at = std::time::SystemTime::now();
            let finished_epoch = unix_seconds(finished_at);
            let duration_seconds = finished_epoch.saturating_sub(started_epoch);
            let target_name = target_name_for(scoped_dir.as_deref(), ctx.working_dir.as_path());
            let title = match kind {
                ArtefactKind::SecurityReview => format!("Security review: {target_name}"),
                ArtefactKind::EvalReport => format!("Eval: {target_name}"),
                ArtefactKind::Image => format!("Image: {target_name}"),
                ArtefactKind::Other => format!("Report: {target_name}"),
            };

            // Pull token stats out of the child's ToolOutput.metadata
            // (stamped by spawn_child) and compute USD cost via the
            // first-class pricing registry.  A model without a known
            // rate card simply omits the cost field rather than faking
            // a zero.
            let input_tokens = out
                .metadata
                .as_ref()
                .and_then(|m| m.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let output_tokens = out
                .metadata
                .as_ref()
                .and_then(|m| m.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let llm_calls = out
                .metadata
                .as_ref()
                .and_then(|m| m.get("llm_calls"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let pricing = crate::llm::pricing::lookup(&self.provider, &self.model);
            let cost_usd = pricing.map(|p| p.cost_usd(input_tokens, output_tokens));

            let mut metadata = serde_json::json!({
                "model": self.model,
                "provider": provider_label(&self.provider),
                "target_name": target_name,
                "target_path": scoped_dir
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ctx.working_dir.display().to_string()),
                "started_at": started_epoch,
                "finished_at": finished_epoch,
                "duration_seconds": duration_seconds,
                "bytes": out.content.len(),
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "llm_calls": llm_calls,
            });
            if let Some(cost) = cost_usd {
                metadata["cost_usd"] = serde_json::json!(cost);
            }
            if let Some(p) = pricing {
                metadata["pricing"] = serde_json::json!({
                    "input_per_mtok": p.input_per_mtok,
                    "output_per_mtok": p.output_per_mtok,
                });
            }

            // Emit a terminal "cost summary" checkpoint — lands in the
            // same SSE stream as the phase checkpoints so the UI chip
            // line shows the final cost even if the user never opens
            // the artefact reader.
            let summary = match cost_usd {
                Some(c) => format!(
                    "{}: {} calls · {} in / {} out · ${:.2}",
                    self.config.name,
                    llm_calls,
                    kfmt(input_tokens),
                    kfmt(output_tokens),
                    c,
                ),
                None => format!(
                    "{}: {} calls · {} in / {} out",
                    self.config.name,
                    llm_calls,
                    kfmt(input_tokens),
                    kfmt(output_tokens),
                ),
            };
            out.checkpoints.push(CheckpointEvent {
                message: summary,
                progress: Some(1.0),
            });

            // For the PenetrationTest harness, upgrade the plain-markdown
            // artefact into a structured security-report: the protocol
            // appends a machine-readable findings block, so persist it as
            // the report document and point the artefact at it via
            // `report_path` — the same contract security_engineer uses, and
            // the field the UI keys the severity-grouped findings card on.
            // Absent or malformed block falls through to plain markdown.
            if self.config.harness == Some(OrchestratorHarness::PenetrationTest)
                && let Some((findings, block)) = extract_pentest_findings(&out.content)
            {
                let run_id = format!("pentester-{started_epoch}");
                let rollup = pentest_severity_rollup(&findings);
                let doc = serde_json::json!({
                    "schema_version": 1,
                    "run_id": run_id,
                    "generated_by": self.config.name,
                    "target": target_name,
                    "model": self.model,
                    "provider": provider_label(&self.provider),
                    "created_at": finished_epoch,
                    "summary": rollup,
                    "findings": findings,
                });
                match write_pentest_report_document(
                    self.workspace.as_ref(),
                    ctx.working_dir.as_path(),
                    &run_id,
                    &doc,
                )
                .await
                {
                    Ok(report_path) => {
                        metadata["report_path"] = serde_json::json!(report_path);
                        metadata["findings_rollup"] = rollup;
                        // Strip the machine block so the markdown fallback
                        // body stays clean prose.
                        out.content.replace_range(block, "");
                        while out.content.ends_with('\n') || out.content.ends_with(' ') {
                            out.content.pop();
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "pentest: report document write failed; keeping markdown artefact"
                        );
                    }
                }
            }

            let artefact =
                Artefact::markdown(kind, title, out.content.clone()).with_metadata(metadata);
            out.artefacts.push(artefact);
        }

        // Close out the Activity row with duration + outcome.  Fallback
        // via Drop is Ok-without-suffix, which loses the duration and
        // misreports an error result — always finish() explicitly.
        if let Some(tok) = activity_token.take() {
            let elapsed = unix_seconds(std::time::SystemTime::now()).saturating_sub(started_epoch);
            let suffix = format!("{elapsed}s");
            if out.is_error {
                tok.finish(crate::controller::ActivityStatus::Err, Some(&suffix));
            } else {
                tok.finish(crate::controller::ActivityStatus::Ok, Some(&suffix));
            }
        }

        Ok(out)
    }
}

/// Format an integer with `k` / `M` suffix for human-readable token
/// counts.  Used in checkpoint messages where real estate is tight.
fn kfmt(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", (n as f64) / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Seconds since UNIX epoch, saturating to 0 if the clock is before 1970.
fn unix_seconds(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Derive a short target name from the scoped path (preferred) or the
/// parent working dir.  Used for the artefact title and metadata.
fn target_name_for(scoped: Option<&std::path::Path>, fallback: &std::path::Path) -> String {
    let path = scoped.unwrap_or(fallback);
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("target")
        .to_string()
}

/// Human-readable provider label for the artefact metadata.
fn provider_label(provider: &LlmProvider) -> String {
    // Debug print gives us "Anthropic", "OpenAi", etc. for the enum —
    // good enough for a metadata string without a new Display impl.
    format!("{provider:?}")
}

/// Extract the machine-readable findings block the pentest protocol appends
/// to its report: the last fenced code block whose body parses as a JSON
/// object carrying a `findings` array. Returns the findings plus the byte
/// range of the whole fenced block so the caller can strip it from the
/// markdown body. Other code fences (curl commands, evidence) don't parse as
/// a findings object and are skipped, so the last real block wins even when
/// the report quotes shell snippets.
fn extract_pentest_findings(
    markdown: &str,
) -> Option<(Vec<serde_json::Value>, std::ops::Range<usize>)> {
    let mut from = 0usize;
    let mut best: Option<(Vec<serde_json::Value>, std::ops::Range<usize>)> = None;
    while let Some(rel) = markdown[from..].find("```") {
        let fence_start = from + rel;
        // Skip past the opening fence's line (which may carry a `json` tag).
        let after_fence = fence_start + 3;
        let line_end = markdown[after_fence..]
            .find('\n')
            .map(|i| after_fence + i + 1)
            .unwrap_or(markdown.len());
        let Some(close_rel) = markdown[line_end..].find("```") else {
            break;
        };
        let body_end = line_end + close_rel;
        let close_end = body_end + 3;
        if let Ok(value) =
            serde_json::from_str::<serde_json::Value>(markdown[line_end..body_end].trim())
            && let Some(arr) = value.get("findings").and_then(serde_json::Value::as_array)
        {
            best = Some((arr.clone(), fence_start..close_end));
        }
        from = close_end;
    }
    best
}

/// Count findings into the four-level severity rollup the UI card and
/// `SeverityRollup` expect. Unknown / `info` severities fold into `low`,
/// matching the reader so a mistyped severity never drops a finding.
fn pentest_severity_rollup(findings: &[serde_json::Value]) -> serde_json::Value {
    let (mut critical, mut high, mut medium, mut low) = (0u32, 0u32, 0u32, 0u32);
    for finding in findings {
        match finding
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "critical" => critical += 1,
            "high" => high += 1,
            "medium" => medium += 1,
            _ => low += 1,
        }
    }
    serde_json::json!({ "critical": critical, "high": high, "medium": medium, "low": low })
}

/// Persist a pentest report document into the Swarm-mirrored `kb/` workspace
/// (or the local `.dyson/` fallback), returning the workspace-relative path
/// stamped into `report_path`. Mirrors
/// `security_engineer::report::save_report_document`; best-effort, callers
/// keep the markdown artefact if it fails.
async fn write_pentest_report_document(
    workspace: Option<&WorkspaceHandle>,
    working_dir: &std::path::Path,
    run_id: &str,
    doc: &serde_json::Value,
) -> std::result::Result<String, String> {
    let rel = format!("kb/security-harness/reports/{run_id}.json");
    let body = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    if let Some(workspace) = workspace {
        let mut guard = workspace.write().await;
        guard.set(&rel, &body);
        guard.save().map_err(|e| e.to_string())?;
        return Ok(rel);
    }
    let path = working_dir
        .join(".dyson")
        .join("security-harness")
        .join("reports")
        .join(format!("{run_id}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&path, body)
        .await
        .map_err(|e| e.to_string())?;
    Ok(rel)
}

/// Parsed input for `OrchestratorTool`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrchestratorInput {
    #[serde(default)]
    pub(crate) task: String,
    #[serde(default)]
    pub(crate) context: String,
    /// Exact active-test target. Used only by the penetration-test contract.
    #[serde(default)]
    pub(crate) target: String,
    /// Operator-supplied assertion of authority over the active-test target.
    #[serde(default)]
    pub(crate) authorization: String,
    /// Technique, rate, exclusion, account, and stop-condition boundaries.
    #[serde(default)]
    pub(crate) rules_of_engagement: String,
    /// Optional directory to scope the child agent to.  See `input_schema`.
    #[serde(default)]
    pub(crate) path: String,
    /// Resume a prior durable harness checkpoint instead of starting a new run.
    #[serde(default)]
    pub(crate) resume: bool,
    /// Optional durable harness run id to resume.
    #[serde(default)]
    pub(crate) run_id: Option<String>,
    /// Optional bounded-run stage used by smoke tests and operator check-pointing.
    #[serde(default)]
    pub(crate) stop_after_stage: Option<String>,
}

/// Collect the active-test boundary directly from the browser operator before
/// a penetration-test child is spawned. Even when the parent model supplied
/// values, they are shown as editable defaults and require explicit UI
/// acceptance. Headless controllers retain the strict inline contract.
async fn complete_pentest_preflight(
    parsed: &mut OrchestratorInput,
) -> std::result::Result<(), String> {
    if !crate::skill::mcp::elicitation::ui_enabled() {
        return Ok(());
    }

    let result = crate::skill::mcp::elicitation::broker()
        .elicit(
            "pentester".to_string(),
            "Penetration-test preflight: confirm the exact target, your authority to test it, \
             and the rules of engagement. No reconnaissance or active requests begin until \
             you accept this scope."
                .to_string(),
            pentest_preflight_schema(parsed),
        )
        .await;
    apply_pentest_preflight_result(parsed, &result)
}

/// Flat JSON Schema supported by Dyson's elicitation form. Existing inline
/// values become defaults so the operator can review and amend them.
pub(crate) fn pentest_preflight_schema(parsed: &OrchestratorInput) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "title": "Authorized target",
                "description": "Exact URL, hostname, IP address/range, or local service. \
                    Redirects, discovered subdomains, adjacent cloud resources, and third-party \
                    integrations are excluded unless explicitly named.",
                "minLength": 1,
                "maxLength": 2048,
                "default": parsed.target
            },
            "authorization": {
                "type": "string",
                "title": "Authorization statement",
                "description": "State that you own the target or have permission from its owner \
                    to perform this penetration test.",
                "minLength": 1,
                "maxLength": 4096,
                "default": parsed.authorization
            },
            "rules_of_engagement": {
                "type": "string",
                "title": "Rules of engagement",
                "description": "Allowed techniques, exclusions, request-rate and concurrency \
                    limits, test accounts, time window, stop conditions, and data-handling rules.",
                "minLength": 1,
                "maxLength": 8192,
                "default": parsed.rules_of_engagement
            }
        },
        "required": ["target", "authorization", "rules_of_engagement"]
    })
}

/// Apply an accepted preflight answer. Cancel, decline, malformed content, or
/// an empty boundary fails closed before the child agent receives any tools.
pub(crate) fn apply_pentest_preflight_result(
    parsed: &mut OrchestratorInput,
    result: &serde_json::Value,
) -> std::result::Result<(), String> {
    if result.get("action").and_then(|value| value.as_str()) != Some("accept") {
        return Err(
            "penetration test cancelled during authorization and scope preflight".to_string(),
        );
    }
    let content = result
        .get("content")
        .and_then(|value| value.as_object())
        .ok_or_else(|| "penetration-test preflight returned no scope information".to_string())?;

    for (name, destination) in [
        ("target", &mut parsed.target),
        ("authorization", &mut parsed.authorization),
        ("rules_of_engagement", &mut parsed.rules_of_engagement),
    ] {
        let value = content
            .get(name)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim();
        if value.is_empty() {
            return Err(format!(
                "{name} is required for penetration testing; no active testing was started"
            ));
        }
        *destination = value.to_string();
    }
    Ok(())
}

#[cfg(test)]
mod pentest_report_tests {
    use super::{extract_pentest_findings, pentest_severity_rollup};

    #[test]
    fn extracts_findings_block_and_ignores_evidence_fences() {
        let md = "# Penetration Test Report\n\nSome prose.\n\n\
```bash\ncurl -s https://target/mcp\n```\n\n\
More prose describing a finding.\n\n\
```json\n{\"findings\":[{\"title\":\"Open DCR\",\"severity\":\"medium\",\
\"vulnerability_class\":\"broken_access_control\"}]}\n```\n";
        let (findings, range) = extract_pentest_findings(md).expect("findings block found");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["title"], "Open DCR");
        // The stripped range must remove the json fence but keep the prose
        // and the unrelated bash fence.
        let mut body = md.to_string();
        body.replace_range(range, "");
        assert!(!body.contains("\"findings\""), "json block stripped");
        assert!(
            body.contains("curl -s https://target/mcp"),
            "evidence fence kept"
        );
        assert!(body.contains("More prose"), "prose kept");
    }

    #[test]
    fn returns_none_without_a_findings_block() {
        let md = "# Report\n\n```json\n{\"notfindings\":1}\n```\n";
        assert!(extract_pentest_findings(md).is_none());
    }

    #[test]
    fn last_findings_block_wins() {
        let md = "```json\n{\"findings\":[]}\n```\nlater\n\
```json\n{\"findings\":[{\"severity\":\"high\"}]}\n```";
        let (findings, _) = extract_pentest_findings(md).expect("block");
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn severity_rollup_buckets_unknown_into_low() {
        let findings = vec![
            serde_json::json!({"severity":"CRITICAL"}),
            serde_json::json!({"severity":"high"}),
            serde_json::json!({"severity":"medium"}),
            serde_json::json!({"severity":"info"}),
            serde_json::json!({"title":"no severity"}),
        ];
        let rollup = pentest_severity_rollup(&findings);
        assert_eq!(rollup["critical"], 1);
        assert_eq!(rollup["high"], 1);
        assert_eq!(rollup["medium"], 1);
        assert_eq!(rollup["low"], 2);
    }
}
