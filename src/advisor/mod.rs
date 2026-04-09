// ===========================================================================
// Advisor — consult a stronger model for strategic guidance.
//
// Two implementations:
//
//   NativeAnthropicAdvisor  — injects `advisor_20260301` into the Anthropic
//                             API request.  Zero-overhead, server-side.
//
//   GenericAdvisor           — registers a Dyson-side `advisor` tool that
//                             spawns a child agent with the parent's tools.
//                             Works with any provider.
//
// Selection logic (in `create_advisor`):
//   executor is Anthropic  →  NativeAnthropicAdvisor
//   otherwise              →  GenericAdvisor
// ===========================================================================

pub mod generic;

use std::sync::Arc;

use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::LlmProvider;
use crate::llm::LlmClient;
use crate::sandbox::Sandbox;
use crate::tool::Tool;

// ---------------------------------------------------------------------------
// Advisor trait
// ---------------------------------------------------------------------------

/// A stronger model the executor can consult for complex decisions.
///
/// Implementations either inject provider-native tool entries into the API
/// request (Anthropic) or register a Dyson-side tool that makes a separate
/// LLM call (generic).
///
/// ## Lifecycle
///
/// 1. `create_advisor()` — construct with model + client
/// 2. `bind()` — called from `Agent::new()` after tools/sandbox/workspace
///    are available, so the advisor tool can inherit the parent's resources
/// 3. `api_tool_entries()` / `tools()` — used during agent construction
pub trait Advisor: Send + Sync {
    /// Bind the advisor to the parent agent's resources.
    ///
    /// Called from `Agent::new()` after the tool registry, sandbox, and
    /// workspace are resolved.  Generic advisors use this to give their
    /// child agent the same capabilities as the parent.
    ///
    /// No-op for native advisors (Anthropic handles everything server-side).
    fn bind(
        &mut self,
        _sandbox: Arc<dyn Sandbox>,
        _workspace: Option<
            std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
        >,
        _inherited_tools: Vec<Arc<dyn Tool>>,
    ) {
    }

    /// Raw JSON tool entries to inject into the API request body.
    ///
    /// Used by providers with native advisor support (Anthropic).
    /// Returns an empty vec for tool-based advisors.
    fn api_tool_entries(&self) -> Vec<serde_json::Value> {
        vec![]
    }

    /// Dyson-side tools this advisor provides.
    ///
    /// Registered alongside regular tools in the agent's tool registry.
    /// Returns an empty vec for native advisors.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

// ---------------------------------------------------------------------------
// NativeAnthropicAdvisor
// ---------------------------------------------------------------------------

/// Advisor that uses Anthropic's server-side `advisor_20260301` tool type.
///
/// The Anthropic Messages API handles everything — the executor model can
/// invoke the advisor during generation, and advisor tokens are billed
/// separately.  No Dyson-side tool execution needed.
struct NativeAnthropicAdvisor {
    model: String,
}

impl Advisor for NativeAnthropicAdvisor {
    fn api_tool_entries(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": "advisor_20260301",
            "name": "advisor",
            "model": self.model,
            "max_uses": 3,
        })]
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create the appropriate advisor for the given executor provider.
///
/// When the executor is Anthropic, returns a `NativeAnthropicAdvisor` that
/// injects the advisor tool into the API request (zero-overhead, server-side).
/// Otherwise, returns a `GenericAdvisor` that registers a Dyson-side tool
/// making a separate LLM call to the advisor model.
pub fn create_advisor(
    executor_provider: &LlmProvider,
    advisor_model: &str,
    client: RateLimitedHandle<Box<dyn LlmClient>>,
) -> Box<dyn Advisor> {
    if *executor_provider == LlmProvider::Anthropic {
        tracing::info!(
            advisor_model = advisor_model,
            "using native Anthropic advisor"
        );
        Box::new(NativeAnthropicAdvisor {
            model: advisor_model.to_string(),
        })
    } else {
        tracing::info!(
            advisor_model = advisor_model,
            "using generic advisor tool"
        );
        Box::new(generic::GenericAdvisor::new(
            advisor_model.to_string(),
            executor_provider.clone(),
            client,
        ))
    }
}
