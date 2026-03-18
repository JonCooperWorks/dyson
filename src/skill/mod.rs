// ===========================================================================
// Skill trait — a pluggable bundle of tools with lifecycle hooks.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines the `Skill` trait — the primary extension point in Dyson.
//   A skill bundles related tools together with optional lifecycle hooks
//   and a system prompt fragment.  The agent loads skills at startup,
//   flattens their tools into a lookup map, and calls lifecycle hooks
//   at the right moments.
//
// Module layout:
//   mod.rs     — Skill trait (this file)
//   builtin.rs — BuiltinSkill wrapping the built-in tools (bash, etc.)
//
// Why skills instead of just tools?
//   Tools are stateless capabilities.  Skills add:
//   - **Grouping**: an MCP server provides 10 tools — they're one skill
//   - **Lifecycle**: on_load (connect to server), on_unload (disconnect)
//   - **Prompting**: skills contribute system prompt fragments so the LLM
//     knows about their tools' conventions and best practices
//   - **Hooks**: before_turn (inject context), after_tool (post-process)
//
// Skill taxonomy:
//
//   BuiltinSkill    — wraps Dyson's built-in tools (bash, read_file, etc.)
//   McpSkill        — connects to an MCP server, wraps each remote tool
//   LocalSkill      — loads a SKILL.md file that defines custom prompts/tools
//
//   The agent loop treats them all identically — it only interacts through
//   the Skill trait.  It doesn't know or care that McpSkill is backed by
//   a subprocess communicating over JSON-RPC.
//
// How skills own tools:
//
//   Skill stores:    Vec<Arc<dyn Tool>>
//   Skill exposes:   fn tools(&self) -> &[Arc<dyn Tool>]
//   Agent clones:    Arc pointers into HashMap<name, Arc<dyn Tool>>
//
//   This gives the agent O(1) lookup by tool name while skills retain
//   ownership for lifecycle management (on_unload needs to know which
//   tools to clean up).
// ===========================================================================

pub mod builtin;
pub mod mcp;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::tool::{Tool, ToolOutput};

// ---------------------------------------------------------------------------
// Skill trait
// ---------------------------------------------------------------------------

/// A pluggable bundle of tools with lifecycle management.
///
/// Implement this trait to extend Dyson with new capabilities.  At minimum,
/// provide a name and a list of tools.  Override the lifecycle hooks as
/// needed for setup, teardown, and per-turn context injection.
///
/// ## Lifecycle
///
/// ```text
/// Agent starts
///   → skill.on_load()           # connect to servers, read configs
///   → skill.tools()             # agent caches Arc<dyn Tool> refs
///   → skill.system_prompt()     # agent composes the full system prompt
///
/// Each LLM turn:
///   → skill.before_turn()       # inject ephemeral context (future)
///   → LLM streams response
///   → for each tool call from this skill:
///       → tool.run(...)
///       → skill.after_tool(name, output)  # post-process results
///
/// Agent shuts down:
///   → skill.on_unload()         # close connections, clean up temp files
/// ```
#[async_trait]
pub trait Skill: Send + Sync {
    /// The skill's unique name, used for logging and identification.
    fn name(&self) -> &str;

    /// The tools this skill provides.
    ///
    /// Called once after `on_load()`.  Returns a slice of `Arc<dyn Tool>`
    /// — the agent clones the Arc pointers (cheap) into its flat lookup
    /// map.  The skill retains ownership of the underlying tools.
    fn tools(&self) -> &[Arc<dyn Tool>];

    /// Optional system prompt fragment.
    ///
    /// Appended to the base system prompt so the LLM knows about this
    /// skill's tools and how to use them.  Return `None` if the tool
    /// descriptions are self-explanatory.
    fn system_prompt(&self) -> Option<&str> {
        None
    }

    /// Called once when the skill is loaded.
    ///
    /// Use this for setup: connecting to MCP servers, reading skill
    /// config files, validating prerequisites.  If this returns an error,
    /// the skill is not loaded and the agent continues without it.
    async fn on_load(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called after a tool from this skill executes.
    ///
    /// Receives the tool name and its output.  Use this for post-processing:
    /// logging, metrics, result augmentation.  The output is immutable here
    /// (the sandbox handles mutation); this is for observation only.
    async fn after_tool(&self, _tool_name: &str, _result: &ToolOutput) -> Result<()> {
        Ok(())
    }

    /// Called on agent shutdown.
    ///
    /// Clean up resources: close MCP connections, kill child processes,
    /// delete temp files.  Errors are logged but don't prevent shutdown.
    async fn on_unload(&mut self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Skill factory — build skills from config.
// ---------------------------------------------------------------------------

/// Create skills from the settings.
///
/// Iterates `settings.skills`, constructs the appropriate Skill impl for
/// each, and calls `on_load()` to initialize them.  MCP skills connect
/// to their servers and discover tools during on_load().
///
/// Skills that fail to load are logged and skipped — the agent continues
/// without them.
pub async fn create_skills(settings: &crate::config::Settings) -> Vec<Box<dyn Skill>> {
    let mut skills: Vec<Box<dyn Skill>> = Vec::new();

    for config in &settings.skills {
        match config {
            crate::config::SkillConfig::Builtin(cfg) => {
                // TODO: respect cfg.tools filter
                let _ = cfg;
                skills.push(Box::new(builtin::BuiltinSkill::new()));
            }
            crate::config::SkillConfig::Mcp(cfg) => {
                let mut mcp_skill = mcp::McpSkill::new(cfg.clone());
                match mcp_skill.on_load().await {
                    Ok(()) => {
                        tracing::info!(
                            server = cfg.name,
                            tools = mcp_skill.tools().len(),
                            "MCP skill loaded"
                        );
                        skills.push(Box::new(mcp_skill));
                    }
                    Err(e) => {
                        tracing::error!(
                            server = cfg.name,
                            error = %e,
                            "MCP skill failed to load — skipping"
                        );
                    }
                }
            }
            crate::config::SkillConfig::Local(_) => {
                tracing::warn!("local skills not yet implemented — skipping");
            }
        }
    }

    skills
}
