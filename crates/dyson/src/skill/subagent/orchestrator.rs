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
use tokio::sync::RwLock;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::workspace::Workspace;

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
    /// Optional post-validator.  Runs on the child's final report text.
    /// Any issues it returns drive the retry loop (see
    /// `max_validator_retries`).  If issues remain after the last retry,
    /// they are appended to the output under a `## Automated Validation
    /// Issues` section so the parent still sees them.
    pub post_validator: Option<fn(&str) -> Vec<String>>,
    /// How many times to re-spawn the child with "fix these issues" when
    /// the validator flags problems.  `0` = no retry (append issues and
    /// return).  Ignored when `post_validator` is `None`.
    pub max_validator_retries: usize,
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
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
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
    pub fn new(
        config: OrchestratorConfig,
        provider: LlmProvider,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn Workspace>>>>,
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

        let user_message = if parsed.context.is_empty() {
            parsed.task
        } else {
            format!("Context:\n{}\n\nTask:\n{}", parsed.context, parsed.task)
        };

        let mut tools =
            Vec::with_capacity(self.direct_tools.len() + self.inner_subagent_tools.len());
        tools.extend(self.direct_tools.iter().cloned());
        tools.extend(self.inner_subagent_tools.iter().cloned());

        let mut output = self.spawn(user_message, tools.clone(), ctx.depth).await?;

        let Some(validator) = self.config.post_validator else {
            return Ok(output);
        };

        let mut retries_left = self.config.max_validator_retries;
        loop {
            let issues = validator(&output.content);
            if issues.is_empty() {
                return Ok(output);
            }
            if retries_left == 0 {
                append_issue_appendix(&mut output.content, &issues);
                return Ok(output);
            }
            retries_left -= 1;
            let msg = build_retry_message(&output.content, &issues);
            output = self.spawn(msg, tools.clone(), ctx.depth).await?;
        }
    }
}

impl OrchestratorTool {
    /// Spawn one child pass with the given user message and tool set.
    async fn spawn(
        &self,
        user_message: String,
        tools: Vec<Arc<dyn Tool>>,
        parent_depth: u8,
    ) -> Result<ToolOutput> {
        let settings = AgentSettings {
            model: crate::llm::registry::lookup(&self.provider)
                .default_model
                .to_string(),
            max_iterations: self.config.max_iterations,
            max_tokens: self.config.max_tokens,
            system_prompt: self.config.system_prompt.to_string(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };
        spawn_child(ChildSpawn {
            name: self.config.name,
            settings,
            inherited_tools: tools,
            sandbox: Arc::clone(&self.sandbox),
            workspace: self.workspace.clone(),
            client: self.client.clone(),
            parent_depth,
            working_dir: None,
            user_message,
        })
        .await
    }
}

fn append_bullets(out: &mut String, issues: &[String]) {
    for issue in issues {
        out.push_str("- ");
        out.push_str(issue);
        out.push('\n');
    }
}

/// Appended to the report when the retry budget is exhausted — keeps the
/// report flowing back to the parent while making the unresolved issues
/// visible.
fn append_issue_appendix(out: &mut String, issues: &[String]) {
    out.push_str(
        "\n\n## Automated Validation Issues\n\n\
         The post-check layer flagged these issues in the report above.  \
         Retry budget exhausted — the report still flows back for visibility.\n\n",
    );
    append_bullets(out, issues);
}

/// User message for a validator-driven retry.  The child starts fresh (no
/// shared chat history) but has its prior report as context and all its
/// tools for re-investigation.  The retry instructions explicitly tell the
/// child to REUSE the prior report's attack-surface enumeration and
/// dependency-review output — without this, each retry re-runs
/// `attack_surface_analyzer` and re-dispatches `dependency_review`, which
/// means a fresh OSV query round-trip and a full manifest walk for every
/// retry.  Only re-run those tools when the specific issue demands it.
fn build_retry_message(prior_report: &str, issues: &[String]) -> String {
    let mut msg = String::from(
        "Your previous report failed the automated post-check.  Fix the issues \
         below and emit a new, complete report in the same format.  Emit the \
         FULL report, not a diff — the caller expects the complete document.\n\n\
         ## Reuse prior work\n\n\
         This is a retry pass.  The prior report below already contains the \
         attack-surface map and dependency_review output from the first pass.  \
         Do NOT re-run `attack_surface_analyzer` or re-dispatch \
         `dependency_review` unless a specific issue requires fresh data \
         (e.g. a coverage-floor gap that names a file not in the prior map).  \
         For each issue, do the minimum targeted work to fix it — typically a \
         focused `read_file` or `ast_query` on the cited line — then re-emit \
         the corrected report with the prior report's unrelated findings \
         preserved.\n\n\
         ## Issues to fix\n\n",
    );
    append_bullets(&mut msg, issues);
    msg.push_str("\n## Your previous report (reuse the attack-surface map and dep_review output verbatim unless a listed issue invalidates them)\n\n");
    msg.push_str(prior_report);
    msg
}

/// Parsed input for `OrchestratorTool`.
#[derive(Debug, Deserialize)]
struct OrchestratorInput {
    task: String,
    #[serde(default)]
    context: String,
}
