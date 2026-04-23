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
use crate::tool::{CheckpointEvent, Tool, ToolContext, ToolOutput};
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
    /// When true, detect languages/frameworks in the scoped review
    /// directory at call time and append matching cheatsheets to the
    /// child's system prompt.  Only `security_engineer` sets this —
    /// other orchestrators (future devops, architect, ...) don't want
    /// vuln cheatsheets.  Detection cost: one shallow directory walk
    /// and 1–3 `toml` / `json` parses per invocation.
    pub inject_cheatsheets: bool,
    /// When set, the child's final text is wrapped as an `Artefact` of
    /// this kind and attached to the returned `ToolOutput` — the HTTP
    /// controller renders it in the Artefacts tab.  The full text is
    /// still returned as `ToolOutput.content` so the parent LLM sees
    /// the report too.
    ///
    /// `None` (the default) keeps the orchestrator chat-only.  The
    /// security engineer opts in; devops/architect/… remain opt-out.
    pub emit_artefact: Option<ArtefactKind>,
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

    fn input_schema(&self) -> serde_json::Value {
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
                "path": {
                    "type": "string",
                    "description": "Optional directory to scope the orchestrator's \
                        child agent to.  When set, the child's working directory is \
                        this path — relative tool paths resolve against it and `bash` \
                        starts here.  Falls back to the parent's working directory \
                        when omitted."
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: OrchestratorInput =
            serde_json::from_value(input.clone()).map_err(|e| {
                DysonError::tool(self.config.name, format!("invalid input: {e}"))
            })?;

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

        let user_message = if parsed.context.is_empty() {
            parsed.task
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

        // Compose the child's system prompt.  Cheatsheets attach only
        // for orchestrators that opt in (security_engineer today).
        // Detection runs against the effective review root — the
        // scoped `path` if provided, else the parent's working dir.
        let mut system_prompt = self.config.system_prompt.to_string();
        let mut active_sheets: Vec<String> = Vec::new();
        if self.config.inject_cheatsheets && cheatsheets_enabled_via_env() {
            let detect_root: &std::path::Path = scoped_dir
                .as_deref()
                .unwrap_or(ctx.working_dir.as_path());
            let (body, sheets) =
                super::repo_detect::detect_and_compose(detect_root);
            if !sheets.is_empty() {
                tracing::info!(
                    tool = self.config.name,
                    sheets = ?sheets,
                    "cheatsheets injected into security_engineer system prompt"
                );
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&body);
                active_sheets = sheets.iter().map(|s| s.to_string()).collect();
            } else {
                tracing::info!(
                    tool = self.config.name,
                    "no cheatsheets matched — injecting none"
                );
            }
        }

        if emits_progress {
            let msg = if active_sheets.is_empty() {
                format!("{}: no language cheatsheets matched", self.config.name)
            } else {
                format!(
                    "{}: cheatsheets loaded ({})",
                    self.config.name,
                    active_sheets.join(", "),
                )
            };
            phase_checkpoints.push(CheckpointEvent { message: msg, progress: Some(0.05) });
            phase_checkpoints.push(CheckpointEvent {
                message: format!("{}: subagent analysing — this may take several minutes", self.config.name),
                progress: Some(0.1),
            });
        }

        let settings = AgentSettings {
            model: self.model.clone(),
            max_iterations: self.config.max_iterations,
            max_tokens: self.config.max_tokens,
            system_prompt,
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

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
        })
        .await;
        let mut out = match spawn_result {
            Ok(o) => o,
            Err(e) => {
                if let Some(tok) = activity_token.take() {
                    tok.finish(
                        crate::controller::ActivityStatus::Err,
                        Some("spawn failed"),
                    );
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
                        self.config.name, elapsed, out.content.len(),
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

            let artefact = Artefact::markdown(kind, title, out.content.clone())
                .with_metadata(metadata);
            out.artefacts.push(artefact);
        }

        // Close out the Activity row with duration + outcome.  Fallback
        // via Drop is Ok-without-suffix, which loses the duration and
        // misreports an error result — always finish() explicitly.
        if let Some(tok) = activity_token.take() {
            let elapsed = unix_seconds(std::time::SystemTime::now())
                .saturating_sub(started_epoch);
            let suffix = format!("{elapsed}s");
            if out.is_error {
                tok.finish(
                    crate::controller::ActivityStatus::Err,
                    Some(&suffix),
                );
            } else {
                tok.finish(
                    crate::controller::ActivityStatus::Ok,
                    Some(&suffix),
                );
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

/// Environment-level kill switch for cheatsheet injection.  The
/// `expensive_live_security_review` example sets this from its
/// `--cheatsheets {on,off}` flag so A/B runs against the same target
/// can measure the effect of the sheets.  Values that read as "off":
/// `off`, `false`, `0`, `no`.  Anything else (including unset) = on.
///
/// Env-var gating keeps the example from having to rebuild or mutate
/// the baked-in `OrchestratorConfig` — the tool is handed back through
/// `create_skills` as an `Arc<dyn Tool>` with no outward config handle.
fn cheatsheets_enabled_via_env() -> bool {
    match std::env::var("DYSON_SECURITY_ENGINEER_CHEATSHEETS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0" | "no"
        ),
        Err(_) => true,
    }
}

/// Parsed input for `OrchestratorTool`.
#[derive(Debug, Deserialize)]
struct OrchestratorInput {
    task: String,
    #[serde(default)]
    context: String,
    /// Optional directory to scope the child agent to.  See `input_schema`.
    #[serde(default)]
    path: String,
}
