// ===========================================================================
// Subagent skill — spawn child agents as tools.
//
// A `SubagentTool` is a Tool that, on invocation, builds a fresh child
// `Agent` with its own LLM client, system prompt, and conversation, runs
// it to completion, and returns the final text as a ToolOutput.
//
//   Parent Agent ──► research_agent (SubagentTool)
//                     └─► Child Agent (runs to completion, returns text)
//
// Invariants:
//   - Shared sandbox: child inherits parent's `Arc<dyn Sandbox>`. Security
//     cannot be bypassed by delegation.
//   - Shared workspace: child sees the same memory files as the parent.
//   - Inherited tools: `Arc<dyn Tool>` clones — no MCP reconnects.
//   - Conversation isolation: only the child's final text reaches the
//     parent; intermediate messages do not.
//   - Recursion cap: subagent tools are excluded from children, with
//     `MAX_SUBAGENT_DEPTH` as a belt-and-suspenders check.
// ===========================================================================

mod coder;
mod orchestrator;
mod repo_detect;
mod security_engineer;

pub use coder::CoderTool;
pub use orchestrator::{OrchestratorConfig, OrchestratorTool};
pub use security_engineer::security_engineer_config;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider, SubagentAgentConfig};
use crate::controller::Output;
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{MAX_SUBAGENT_DEPTH, Tool, ToolContext, ToolOutput};
use crate::workspace::WorkspaceHandle;

/// An `Output` that accumulates a child agent's streamed text into a
/// buffer.  Tool events are logged at `debug` and (when the parent
/// controller wired in a `SubagentEventBus`) tee'd to the parent's SSE
/// stream tagged with `parent_tool_id`, so the browser can render
/// nested tool chips inside the subagent panel as they happen.  Only
/// the final text (and side-channel artefacts) reach the parent's LLM
/// conversation.
///
/// ## LLM boundary vs. UI sink — design note
///
/// Two distinct things are called "the parent": the parent's **LLM
/// conversation** (what the parent model sees in its next turn) and
/// the parent's **UI stream** (what the browser / Telegram client
/// renders).  These are NOT the same channel, and the distinction
/// matters for context budget and privacy.
///
/// - **LLM boundary** (enforced here): only the child's final text and
///   its buffered artefacts flow up.  The child's intermediate tool
///   calls, tool results, thinking fragments, and LLM errors are
///   swallowed deliberately — injecting them into the parent's
///   conversation would blow past the context window after two or
///   three subagent calls and leak implementation noise into the
///   parent's reasoning.  `tool_use_start` / `tool_result` / `error`
///   here never append to `self.text` — that text is what the parent
///   LLM reads back.
///
/// - **UI sink**: when the optional `events` bus is wired (HTTP
///   controller only), the same `tool_use_start` / `tool_result` /
///   `send_file` / `send_artefact` calls also tee a tagged `SseEvent`
///   to the parent's per-chat broadcast channel via the bus.  Those
///   events carry `parent_tool_id` (the spawning tool's id, supplied
///   by `ChildSpawn.parent_tool_id`) so the frontend can attach them
///   to the right subagent panel.  The bus is fire-and-forget — slow
///   subscribers, missing receivers, and reconnects degrade
///   gracefully (no replay-ring entry; the panel just looks empty
///   until the next nested event arrives).  This path never flows
///   into any LLM prompt — it's pure UI.
pub struct CaptureOutput {
    text: String,
    artefacts: Vec<crate::message::Artefact>,
    /// Optional UI side-channel.  `None` for non-HTTP controllers and
    /// for tests; `Some` when the HTTP controller wired
    /// `Agent::set_subagent_events` for this turn.
    events: Option<crate::controller::http::SubagentEventBus>,
    /// The parent agent's `tool_use_id` for the subagent tool call
    /// that produced this child.  Used as the frontend's
    /// `parent_tool_id` tag.  `None` disables tee'ing even when
    /// `events` is set — there's no panel to attach inner events to
    /// without a parent id.
    parent_tool_id: Option<String>,
    /// Tracks the current inner tool call's id so `tool_result` can
    /// emit it as `tool_use_id` on the SSE frame.  Set in
    /// `tool_use_start`, cleared in `flush`.  Mirrors the same field
    /// on `SseOutput` — same reason: file / artefact emissions that
    /// follow a tool result need the originating call's id.
    current_inner_tool_id: Option<String>,
}

impl Default for CaptureOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureOutput {
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            artefacts: Vec::new(),
            events: None,
            parent_tool_id: None,
            current_inner_tool_id: None,
        }
    }

    /// Wire the optional UI bus + parent id.  Both must be set for any
    /// tee'ing to happen — without `parent_tool_id` the frontend has
    /// no panel to attach inner events to, and without `events` there's
    /// no broadcast channel to send through.
    pub fn with_ui_sink(
        mut self,
        events: Option<crate::controller::http::SubagentEventBus>,
        parent_tool_id: Option<String>,
    ) -> Self {
        self.events = events;
        self.parent_tool_id = parent_tool_id;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Drain the buffered artefacts out of the capture.  Called by
    /// `spawn_child` after the child agent completes so the artefacts
    /// bubble up to the parent controller.
    pub fn take_artefacts(&mut self) -> Vec<crate::message::Artefact> {
        std::mem::take(&mut self.artefacts)
    }

    /// Return `Some((bus, parent_id))` if both halves of the UI sink
    /// are populated.  Inline so the hot path stays a single branch.
    fn ui_sink(&self) -> Option<(&crate::controller::http::SubagentEventBus, &str)> {
        match (self.events.as_ref(), self.parent_tool_id.as_deref()) {
            (Some(bus), Some(parent)) => Some((bus, parent)),
            _ => None,
        }
    }
}

impl Output for CaptureOutput {
    fn text_delta(&mut self, text: &str) -> Result<()> {
        self.text.push_str(text);
        Ok(())
    }

    fn tool_use_start(&mut self, id: &str, name: &str) -> Result<()> {
        tracing::debug!(tool = name, id = id, "subagent tool call started");
        self.current_inner_tool_id = Some(id.to_string());
        if let Some((bus, parent)) = self.ui_sink() {
            bus.tool_start(parent, id, name);
        }
        Ok(())
    }

    fn tool_use_complete(&mut self) -> Result<()> {
        Ok(())
    }

    fn tool_result(&mut self, output: &ToolOutput) -> Result<()> {
        tracing::debug!(
            is_error = output.is_error,
            content_len = output.content.len(),
            "subagent tool result"
        );
        if let Some((bus, parent)) = self.ui_sink() {
            bus.tool_result(parent, self.current_inner_tool_id.as_deref(), output);
        }
        Ok(())
    }

    fn send_file(&mut self, path: &std::path::Path) -> Result<()> {
        tracing::debug!(path = %path.display(), "subagent file send (ignored in capture)");
        // We deliberately don't broadcast here: the file's bytes haven't
        // been mirrored into the controller's `FileStore`, so a `file`
        // event with a real `/api/files/<id>` URL only makes sense once
        // the parent's `SseOutput::send_file` runs after the subagent
        // returns.  The replay path (chat reload) reconstructs file
        // blocks from disk anyway.
        Ok(())
    }

    fn send_artefact(&mut self, artefact: &crate::message::Artefact) -> Result<()> {
        tracing::debug!(
            kind = ?artefact.kind,
            title = %artefact.title,
            bytes = artefact.content.len(),
            "subagent artefact buffered for parent",
        );
        // We deliberately don't tee the artefact through the UI bus
        // here.  The bus would emit an `Artefact` event with an empty
        // id/url (no `ArtefactStore` entry exists yet — that's minted
        // upstream by `SseOutput::send_artefact` when this artefact
        // bubbles back through `ToolOutput.artefacts`).  Empty-id
        // events confuse the frontend's chip click-through, and the
        // user-visible delta is small (a few seconds earlier preview
        // chip).  Inner tool_use_start/tool_result tees give plenty
        // of progress signal already.  Keep the bus method around for
        // a future "preview chip" UI iteration.
        self.artefacts.push(artefact.clone());
        Ok(())
    }

    fn error(&mut self, error: &DysonError) -> Result<()> {
        tracing::warn!(error = %error, "subagent error");
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.current_inner_tool_id = None;
        Ok(())
    }
}

/// A `Skill` that wraps pre-loaded `Arc<dyn Tool>` pointers for a child
/// agent.  Unlike `BuiltinSkill` or `McpSkill`, it doesn't own tools —
/// it forwards clones from the parent, so there are no lifecycle hooks
/// and MCP connections aren't re-established.
pub struct FilteredSkill {
    tools: Vec<Arc<dyn Tool>>,
}

impl FilteredSkill {
    /// Exposed for integration tests; production callers go through
    /// `SubagentSkill::new` or `CoderTool`, which wrap this internally.
    #[doc(hidden)]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl Skill for FilteredSkill {
    fn name(&self) -> &str {
        "inherited"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}

/// Arguments for [`spawn_child`].  `SubagentTool` and `CoderTool` each
/// build one of these and delegate the common lifecycle.
pub(crate) struct ChildSpawn<'a> {
    pub name: &'a str,
    pub settings: AgentSettings,
    pub inherited_tools: Vec<Arc<dyn Tool>>,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Option<WorkspaceHandle>,
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    /// Depth of the calling parent; the child runs at `parent_depth + 1`.
    pub parent_depth: u8,
    /// Override the child's working directory (used by `CoderTool`).
    pub working_dir: Option<PathBuf>,
    pub user_message: String,
    /// Parent's activity handle, forwarded so the child's tool calls
    /// bump the parent's Activity tab liveness.  `None` on non-HTTP
    /// controllers.
    pub activity: Option<crate::controller::ActivityHandle>,
    /// Optional UI events bus, forwarded to the child's `CaptureOutput`
    /// so its inner tool calls tee live SSE frames to the browser
    /// tagged with `parent_tool_id`.  `None` on non-HTTP controllers
    /// or when the parent's tool dispatch didn't carry an id (tests).
    pub events: Option<crate::controller::http::SubagentEventBus>,
    /// The parent agent's `tool_use_id` for the subagent tool call
    /// that's spawning this child.  Tagged onto every tee'd UI event
    /// so the frontend attaches inner chips to the correct subagent
    /// panel.
    pub parent_tool_id: Option<String>,
}

/// Build a child `Agent` from `spec`, run it to completion under a
/// `CaptureOutput`, and return its final text as a `ToolOutput`.
///
/// Depth overflow is returned as `ToolOutput::error` (recoverable) rather
/// than `Err`, matching the codebase's split between bad input and
/// runtime failure.
pub(crate) async fn spawn_child(spec: ChildSpawn<'_>) -> Result<ToolOutput> {
    if spec.parent_depth >= MAX_SUBAGENT_DEPTH {
        return Ok(ToolOutput::error(format!(
            "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached. \
             Cannot spawn another subagent."
        )));
    }

    tracing::info!(
        tool = spec.name,
        depth = spec.parent_depth + 1,
        model = spec.settings.model.as_str(),
        "spawning child agent"
    );

    let skills: Vec<Box<dyn Skill>> =
        vec![Box::new(FilteredSkill::new(spec.inherited_tools))];

    let mut builder = crate::agent::Agent::builder(spec.client, spec.sandbox)
        .skills(skills)
        .settings(&spec.settings);
    if let Some(ws) = spec.workspace {
        builder = builder.workspace(ws);
    }
    let mut child_agent = builder.build()?;

    child_agent.set_depth(spec.parent_depth + 1);
    if let Some(dir) = spec.working_dir {
        child_agent.set_working_dir(dir);
    }
    if let Some(handle) = spec.activity {
        child_agent.set_activity_handle(handle);
    }

    // Wire the optional UI sink before running.  Both `events` and
    // `parent_tool_id` must be `Some` for any tee'ing to happen.
    let mut capture = CaptureOutput::new().with_ui_sink(spec.events, spec.parent_tool_id);
    match child_agent.run(&spec.user_message, &mut capture).await {
        Ok(final_text) => {
            let buffered = capture.take_artefacts();
            // Snapshot token usage from the child before it's dropped so
            // the orchestrator can surface a cost line in the artefact
            // metadata.  Budget lives on Conversation so stats persist
            // if the child is kept around for a future turn (it isn't
            // currently, but the accessor is cheap).
            let budget = child_agent.token_budget();
            let stats = serde_json::json!({
                "input_tokens": budget.input_tokens_used,
                "output_tokens": budget.output_tokens_used,
                "llm_calls": budget.llm_calls,
            });
            tracing::info!(
                tool = spec.name,
                result_len = final_text.len(),
                artefacts = buffered.len(),
                input_tokens = budget.input_tokens_used,
                output_tokens = budget.output_tokens_used,
                llm_calls = budget.llm_calls,
                "child agent completed successfully"
            );
            let mut out = ToolOutput::success(final_text);
            out.artefacts = buffered;
            out.metadata = Some(stats);
            Ok(out)
        }
        Err(e) => {
            tracing::warn!(
                tool = spec.name,
                error = %e,
                "child agent failed"
            );
            Ok(ToolOutput::error(format!(
                "Subagent '{}' failed: {e}",
                spec.name,
            )))
        }
    }
}

/// A `Tool` that spawns a child `Agent` per invocation and returns its
/// final text.  Input schema: `{ task: string, context?: string }`.
///
/// The child inherits the parent's sandbox, workspace, and a filtered
/// slice of the parent's tools.  See the module header for invariants.
pub struct SubagentTool {
    config: SubagentAgentConfig,
    /// Resolved provider type.
    provider: LlmProvider,
    /// Fallback model used when `config.model` is `None`.  Resolved at
    /// skill-construction time from the user's config — either the
    /// parent agent's model (for the `"default"` sentinel provider) or
    /// the first entry in the subagent provider's `models` list.  Never
    /// a registry default, so subagents always bill the same model the
    /// user configured.
    parent_model: String,
    /// Shares the parent's rate-limit window via `ClientRegistry`.
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<WorkspaceHandle>,
    /// `Arc<dyn Tool>` clones from the parent's already-loaded skills;
    /// MCP connections remain owned by the parent's `McpSkill`.
    inherited_tools: Vec<Arc<dyn Tool>>,
}

impl SubagentTool {
    pub fn new(
        config: SubagentAgentConfig,
        provider: LlmProvider,
        parent_model: String,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<WorkspaceHandle>,
        inherited_tools: Vec<Arc<dyn Tool>>,
    ) -> Self {
        Self {
            config,
            provider,
            parent_model,
            client,
            sandbox,
            workspace,
            inherited_tools,
        }
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the subagent to complete."
                },
                "context": {
                    "type": "string",
                    "description": "Optional background context to help the subagent understand the task."
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: SubagentInput = serde_json::from_value(input.clone()).map_err(|e| {
            DysonError::tool(&self.config.name, format!("invalid input: {e}"))
        })?;

        let user_message = if parsed.context.is_empty() {
            parsed.task
        } else {
            format!("Context:\n{}\n\nTask:\n{}", parsed.context, parsed.task)
        };

        let model = self
            .config
            .model
            .clone()
            .unwrap_or_else(|| self.parent_model.clone());

        let settings = AgentSettings {
            model,
            max_iterations: self.config.max_iterations.unwrap_or(10),
            max_tokens: self.config.max_tokens.unwrap_or(4096),
            system_prompt: self.config.system_prompt.clone(),
            provider: self.provider.clone(),
            // api_key/base_url are unused — the client handle is pre-authenticated.
            ..AgentSettings::default()
        };

        // Activity tab registration — UI-only side channel.  `None`
        // on non-HTTP controllers.  See `ToolContext::activity` for
        // the LLM-boundary note.
        let mut activity_token = ctx.activity.as_ref().map(|a| {
            a.start(
                crate::controller::LANE_SUBAGENT,
                &self.config.name,
                &crate::controller::truncate_note(&user_message, 80),
            )
        });
        let started_at = std::time::SystemTime::now();

        let result = spawn_child(ChildSpawn {
            name: &self.config.name,
            settings,
            inherited_tools: self.inherited_tools.clone(),
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth: ctx.depth,
            // Inherit the caller's scope.  When a SubagentTool is dispatched
            // from inside an OrchestratorTool's child (the common case —
            // e.g. `security_engineer` dispatching `dependency_review`),
            // the orchestrator has already scoped `ctx.working_dir` to the
            // review root, and the inner subagent needs to see the same
            // root to reach the target's lockfiles / manifests / source.
            // Passing None here silently drops the scope and the child
            // falls back to the process cwd, where it may pick up stale
            // checkouts from prior smoke runs.
            working_dir: Some(ctx.working_dir.clone()),
            user_message,
            activity: ctx.activity.clone(),
            events: ctx.subagent_events.clone(),
            parent_tool_id: ctx.tool_use_id.clone(),
        })
        .await;

        if let Some(tok) = activity_token.take() {
            let elapsed = std::time::SystemTime::now()
                .duration_since(started_at)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let suffix = format!("{elapsed}s");
            let status = match &result {
                Ok(out) if !out.is_error => crate::controller::ActivityStatus::Ok,
                _ => crate::controller::ActivityStatus::Err,
            };
            tok.finish(status, Some(&suffix));
        }
        result
    }
}

/// Parsed input for `SubagentTool`.  Mirrors `input_schema()`.
#[derive(Debug, Deserialize)]
struct SubagentInput {
    task: String,
    #[serde(default)]
    context: String,
}

/// Bundles `SubagentTool` instances (plus the built-in `CoderTool`) into
/// a `Skill` and contributes a system-prompt fragment listing them.
///
/// Built by the parent *after* all other skills load, so it can clone
/// `Arc<dyn Tool>` pointers from their already-initialized tool lists.
pub struct SubagentSkill {
    tools: Vec<Arc<dyn Tool>>,
    system_prompt: String,
}

impl SubagentSkill {
    /// `configs` resolve against `settings.providers`; the sentinel
    /// `"default"` uses the parent's active provider so built-ins work
    /// with no extra config.
    pub fn new(
        configs: &[SubagentAgentConfig],
        settings: &crate::config::Settings,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<WorkspaceHandle>,
        parent_tools: &[Arc<dyn Tool>],
        registry: &crate::controller::ClientRegistry,
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut prompt_lines: Vec<String> = Vec::new();

        for cfg in configs {
            let (provider, parent_model, client) = if cfg.provider == "default" {
                (
                    settings.agent.provider.clone(),
                    settings.agent.model.clone(),
                    registry.get_default(),
                )
            } else {
                match settings.providers.get(&cfg.provider) {
                    Some(pc) => match registry.get(&cfg.provider) {
                        Ok(handle) => (
                            pc.provider_type.clone(),
                            pc.default_model().to_string(),
                            handle,
                        ),
                        Err(e) => {
                            tracing::error!(
                                subagent = cfg.name,
                                provider = cfg.provider,
                                error = %e,
                                "failed to create client for subagent — skipping"
                            );
                            continue;
                        }
                    },
                    None => {
                        tracing::error!(
                            subagent = cfg.name,
                            provider = cfg.provider,
                            "unknown provider for subagent — skipping"
                        );
                        continue;
                    }
                }
            };

            let inherited = filter_tools_checked(&cfg.name, parent_tools, &cfg.tools);

            let tool = SubagentTool::new(
                cfg.clone(),
                provider,
                parent_model,
                client,
                Arc::clone(&sandbox),
                workspace.clone(),
                inherited,
            );

            let tool_names: Vec<&str> = tool.inherited_tools.iter().map(|t| t.name()).collect();
            tracing::info!(
                subagent = cfg.name,
                provider = cfg.provider,
                tools = ?tool_names,
                "subagent configured"
            );

            prompt_lines.push(format!("- **{}**: {}", cfg.name, cfg.description,));

            tools.push(Arc::new(tool));
        }

        // Coder is always present: it takes a `path` and scopes the
        // child's `working_dir`, behavior SubagentTool doesn't expose.
        // Built-in tools (coder, orchestrators, inner subagents) all
        // inherit the parent's configured model — never a registry default.
        let coder_provider = settings.agent.provider.clone();
        let coder_model = settings.agent.model.clone();
        let coder_client = registry.get_default();
        let coder_tool = CoderTool::new(
            coder_provider.clone(),
            coder_model.clone(),
            coder_client.clone(),
            Arc::clone(&sandbox),
            workspace.clone(),
            parent_tools,
        );
        prompt_lines.push(format!("- **{}**: {}", coder_tool.name(), coder_tool.description()));
        tools.push(Arc::new(coder_tool));

        // Orchestrators: composable subagents that get direct tools + inner
        // subagent dispatch.  Each is defined by an OrchestratorConfig.
        let orch_configs = builtin_orchestrator_configs();
        if !orch_configs.is_empty() {
            // Build inner subagent tools once — shared across all orchestrators.
            let builtin_configs = builtin_subagent_configs();
            let mut inner_subagent_tools: Vec<Arc<dyn Tool>> = Vec::new();

            for cfg in &builtin_configs {
                let inherited = filter_tools(parent_tools, &cfg.tools);
                let inner_tool = SubagentTool::new(
                    cfg.clone(),
                    settings.agent.provider.clone(),
                    settings.agent.model.clone(),
                    registry.get_default(),
                    Arc::clone(&sandbox),
                    workspace.clone(),
                    inherited,
                );
                inner_subagent_tools.push(Arc::new(inner_tool));
            }

            // Inner coder shared by all orchestrators.
            let inner_coder = CoderTool::new(
                settings.agent.provider.clone(),
                settings.agent.model.clone(),
                registry.get_default(),
                Arc::clone(&sandbox),
                workspace.clone(),
                parent_tools,
            );
            inner_subagent_tools.push(Arc::new(inner_coder));

            // Clone inner tools for all but the last orchestrator,
            // then move the vec into the final one to avoid an extra clone.
            let orch_count = orch_configs.len();
            for (i, orch_cfg) in orch_configs.iter().enumerate() {
                let inner = if i + 1 < orch_count {
                    inner_subagent_tools.clone()
                } else {
                    std::mem::take(&mut inner_subagent_tools)
                };
                let orch_tool = OrchestratorTool::new(
                    orch_cfg.clone(),
                    coder_provider.clone(),
                    coder_model.clone(),
                    coder_client.clone(),
                    Arc::clone(&sandbox),
                    workspace.clone(),
                    parent_tools,
                    inner,
                );
                prompt_lines.push(format!(
                    "- **{}**: {}",
                    orch_tool.name(),
                    orch_tool.description()
                ));
                tools.push(Arc::new(orch_tool));
            }
        }

        // Subagents and orchestrators may contribute usage-protocol
        // fragments to the parent's system prompt.
        let system_prompt = if prompt_lines.is_empty() {
            String::new()
        } else {
            let mut prompt = format!(
                "You have access to the following subagents — specialized AI agents \
                 that you can delegate tasks to.  Each subagent may use a different \
                 model and has its own expertise.  Use them when their specialty \
                 matches the task:\n\n{}",
                prompt_lines.join("\n")
            );

            // Protocol fragments from config-driven subagents.
            for cfg in configs {
                if let Some(ref fragment) = cfg.injects_protocol {
                    prompt.push_str(fragment);
                }
            }

            // Protocol fragments from orchestrators.
            for orch_cfg in &orch_configs {
                if let Some(fragment) = orch_cfg.injects_protocol {
                    prompt.push_str(fragment);
                }
            }

            prompt
        };

        Self {
            tools,
            system_prompt,
        }
    }
}

#[async_trait]
impl Skill for SubagentSkill {
    fn name(&self) -> &str {
        "subagents"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    fn system_prompt(&self) -> Option<&str> {
        if self.system_prompt.is_empty() {
            None
        } else {
            Some(&self.system_prompt)
        }
    }
}

/// Built-in subagents that ship with every Dyson instance.  All use the
/// `"default"` provider (the parent's own).  Users can extend or
/// override these in `dyson.json`.
///
/// - `planner`: read-only; produces ordered implementation steps.
/// - `researcher`: broad read access including `bash` and `web_search`.
/// - `verifier`: adversarial validation; injects a usage protocol into
///   the parent's system prompt via `injects_protocol`.
pub fn builtin_subagent_configs() -> Vec<SubagentAgentConfig> {
    vec![
        SubagentAgentConfig {
            name: "planner".into(),
            description: "Breaks down complex tasks into concrete, ordered implementation \
                steps.  Reads the codebase to understand structure before planning.  \
                Returns a numbered plan with file paths and specific changes needed."
                .into(),
            system_prompt: include_str!("prompts/planner.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(15),
            max_tokens: Some(4096),
            tools: Some(vec![
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
            ]),
            injects_protocol: None,
        },
        SubagentAgentConfig {
            name: "researcher".into(),
            description: "Does deep research and summarizes findings.  Can read code, \
                run commands, and search the web.  Returns a concise summary of what \
                it found.  Use for questions that need investigation."
                .into(),
            system_prompt: include_str!("prompts/researcher.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(20),
            max_tokens: Some(4096),
            tools: Some(vec![
                "bash".into(),
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
                "web_search".into(),
            ]),
            injects_protocol: None,
        },
        SubagentAgentConfig {
            name: "verifier".into(),
            description: "Adversarial verification agent.  Attempts to break an \
                implementation by running tests, checking edge cases, and validating \
                that changes meet the original specification.  Returns a structured \
                verdict: PASS, FAIL, or PARTIAL with proof of execution.  Use after \
                completing non-trivial changes (3+ files, backend/API logic, or \
                infrastructure modifications)."
                .into(),
            system_prompt: include_str!("prompts/verifier.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(25),
            max_tokens: Some(8192),
            tools: Some(vec![
                "bash".into(),
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
            ]),
            injects_protocol: Some(include_str!("prompts/verifier_protocol.md").into()),
        },
        SubagentAgentConfig {
            name: "dependency_review".into(),
            description: "Finds dependency manifests/lockfiles in the repo, queries \
                Google's OSV database for known vulnerabilities, and returns a \
                prioritized summary grounded in the codebase.  Supports every OSV \
                ecosystem (Cargo, npm, PyPI, Go, Maven, NuGet, RubyGems, Packagist, \
                Pub, Hex, CRAN, SwiftURL, GitHub Actions, Hackage, ConanCenter, \
                and any ecosystem via CycloneDX/SPDX SBOMs); flags unsupported \
                manifests explicitly rather than guessing.  Use for pre-release \
                checks, PR review, and supply-chain triage."
                .into(),
            system_prompt: include_str!("prompts/dependency_review.md").into(),
            provider: "default".into(),
            model: None,
            max_iterations: Some(15),
            max_tokens: Some(6144),
            tools: Some(vec![
                "dependency_scan".into(),
                "read_file".into(),
                "search_files".into(),
                "list_files".into(),
                "bash".into(),
            ]),
            injects_protocol: None,
        },
    ]
}

/// Built-in orchestrator configs.  Each is composed into an
/// `OrchestratorTool` with inner subagent dispatch.  Add new orchestrator
/// roles here — they automatically get planner/researcher/coder/verifier
/// as inner subagents.
pub fn builtin_orchestrator_configs() -> Vec<OrchestratorConfig> {
    vec![security_engineer_config()]
}

/// Filter `parent_tools` by the subagent's optional `tools` list.
/// `None` inherits everything; unknown names are silently dropped (use
/// [`filter_tools_checked`] for a warning).
fn filter_tools(
    parent_tools: &[Arc<dyn Tool>],
    filter: &Option<Vec<String>>,
) -> Vec<Arc<dyn Tool>> {
    match filter {
        None => parent_tools.to_vec(),
        Some(names) => parent_tools
            .iter()
            .filter(|t| names.iter().any(|n| n == t.name()))
            .cloned()
            .collect(),
    }
}

/// [`filter_tools`] with a `warn!` for any filter entry that doesn't
/// match a real parent tool, so operators see why a configured tool is
/// missing.
fn filter_tools_checked(
    subagent_name: &str,
    parent_tools: &[Arc<dyn Tool>],
    filter: &Option<Vec<String>>,
) -> Vec<Arc<dyn Tool>> {
    if let Some(names) = filter {
        let known: std::collections::HashSet<&str> =
            parent_tools.iter().map(|t| t.name()).collect();
        let missing: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| !known.contains(n))
            .collect();
        if !missing.is_empty() {
            tracing::warn!(
                subagent = subagent_name,
                missing = ?missing,
                "subagent tool filter references unknown tools — they will be dropped"
            );
        }
    }
    filter_tools(parent_tools, filter)
}

#[cfg(test)]
mod tests;
