// ===========================================================================
// Generic advisor — a subagent that consults a stronger LLM.
//
// Used when the executor is NOT Anthropic (so we can't use the native
// advisor_20260301 API tool).  Registers an `advisor` tool that spawns
// a child agent with the parent's tools, sandbox, and workspace — just
// like a SubagentTool but with an advisor-specific system prompt.
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::{DysonError, Result};
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};

use super::Advisor;

// ---------------------------------------------------------------------------
// AdvisorTool — a subagent tool that consults the advisor model
// ---------------------------------------------------------------------------

const ADVISOR_SYSTEM_PROMPT: &str = "\
You are a senior advisor providing strategic guidance to another AI agent. \
The agent is working on a task and has consulted you because it faces a \
complex decision. You have full access to the same tools as the requesting \
agent — use them to investigate the codebase, read files, and gather \
context before giving advice. Provide clear, actionable recommendations. \
Focus on the best approach and explain your reasoning.";

pub(crate) struct AdvisorTool {
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    model: String,
    provider: LlmProvider,
    sandbox: Arc<dyn Sandbox>,
    workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
    inherited_tools: Vec<Arc<dyn Tool>>,
}

/// Minimal skill wrapper for inherited tools (same as SubagentTool's approach).
struct InheritedSkill {
    tools: Vec<Arc<dyn Tool>>,
}

#[async_trait]
impl Skill for InheritedSkill {
    fn name(&self) -> &str {
        "inherited"
    }
    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}

#[async_trait]
impl Tool for AdvisorTool {
    fn name(&self) -> &str {
        "advisor"
    }

    fn description(&self) -> &str {
        "Consult a more capable model for strategic guidance on complex \
         decisions. Use when facing architectural choices, ambiguous \
         requirements, or problems that benefit from deeper reasoning. \
         The advisor has access to the same tools as you and can read \
         files, search code, etc. Include relevant context in your query."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Your question for the advisor. Include relevant context about what you're working on and what decision you need help with."
                }
            },
            "required": ["query"]
        })
    }

    fn agent_only(&self) -> bool {
        true
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| DysonError::tool("advisor", "missing required 'query' field"))?;

        tracing::info!(
            model = self.model,
            depth = ctx.depth + 1,
            "spawning advisor subagent"
        );

        let child_settings = AgentSettings {
            model: self.model.clone(),
            max_iterations: 15,
            max_tokens: 8192,
            system_prompt: ADVISOR_SYSTEM_PROMPT.to_string(),
            provider: self.provider.clone(),
            ..AgentSettings::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(InheritedSkill {
            tools: self.inherited_tools.clone(),
        })];

        let mut builder =
            crate::agent::Agent::builder(self.client.clone(), Arc::clone(&self.sandbox))
                .skills(skills)
                .settings(&child_settings);
        if let Some(ws) = &self.workspace {
            builder = builder.workspace(Arc::clone(ws));
        }
        let mut child_agent = builder.build()?;
        child_agent.set_depth(ctx.depth + 1);

        let mut capture = crate::skill::subagent::CaptureOutput::new();
        match child_agent.run(query, &mut capture).await {
            Ok(final_text) => {
                tracing::info!(
                    result_len = final_text.len(),
                    "advisor subagent completed"
                );
                Ok(ToolOutput::success(final_text))
            }
            Err(e) => {
                tracing::warn!(error = %e, "advisor subagent failed");
                Ok(ToolOutput::error(format!("Advisor failed: {e}")))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GenericAdvisor — wraps AdvisorTool into the Advisor trait
// ---------------------------------------------------------------------------

pub struct GenericAdvisor {
    client: RateLimitedHandle<Box<dyn LlmClient>>,
    model: String,
    provider: LlmProvider,
    tool: Option<Arc<dyn Tool>>,
}

impl GenericAdvisor {
    pub fn new(
        model: String,
        provider: LlmProvider,
        client: RateLimitedHandle<Box<dyn LlmClient>>,
    ) -> Self {
        Self {
            client,
            model,
            provider,
            tool: None,
        }
    }
}

impl Advisor for GenericAdvisor {
    fn bind(
        &mut self,
        sandbox: Arc<dyn Sandbox>,
        workspace: Option<Arc<RwLock<Box<dyn crate::workspace::Workspace>>>>,
        inherited_tools: Vec<Arc<dyn Tool>>,
    ) {
        self.tool = Some(Arc::new(AdvisorTool {
            client: self.client.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            sandbox,
            workspace,
            inherited_tools,
        }));
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        match &self.tool {
            Some(t) => vec![t.clone()],
            None => {
                tracing::warn!("GenericAdvisor::tools() called before bind()");
                vec![]
            }
        }
    }
}
