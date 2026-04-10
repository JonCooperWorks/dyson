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
pub mod local;
pub mod mcp;
pub mod subagent;

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
///   → skill.before_turn()       # inject ephemeral context
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

    /// Called before each LLM turn.
    ///
    /// Use this to inject ephemeral context that changes between turns:
    /// refreshing auth tokens, updating time-sensitive data, or syncing
    /// external state.  Returns an optional string that the agent appends
    /// to the system prompt for that turn only.
    async fn before_turn(&self) -> Result<Option<String>> {
        Ok(None)
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

/// Create skills from settings and (optionally) workspace discovery.
///
/// Iterates `settings.skills`, constructs the appropriate Skill impl for
/// each, and calls `on_load()` to initialize them.  MCP skills connect
/// to their servers and discover tools during on_load().
///
/// When a workspace is provided, also auto-discovers skill files from
/// the workspace's `skills/` directory (Hermes-style).  Workspace skills
/// are loaded after config-defined skills.
///
/// Skills that fail to load are logged and skipped — the agent continues
/// without them.
/// Create skills from settings and (optionally) workspace discovery.
///
/// Uses **two-phase construction** for subagent support:
///
/// 1. **Phase A**: Load all non-subagent skills (builtin, MCP, local).
///    These are the regular skills that provide tools.
///
/// 2. **Phase B**: Flatten the loaded tools from Phase A into a single
///    list, then construct SubagentSkill with those tools.  Each subagent
///    gets `Arc<dyn Tool>` clones of the parent's tools — no duplication,
///    no reconnecting to MCP servers.
///
/// This ordering avoids the chicken-and-egg problem: subagent tools need
/// the parent's tools to exist first, but they're also part of the skill
/// list that feeds the parent.
pub async fn create_skills(
    settings: &crate::config::Settings,
    workspace: Option<&dyn crate::workspace::Workspace>,
    sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox>,
    workspace_arc: Option<
        std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
    >,
    registry: &crate::controller::ClientRegistry,
) -> Vec<Box<dyn Skill>> {
    let mut skills: Vec<Box<dyn Skill>> = Vec::new();
    let mut subagent_configs: Vec<crate::config::SubagentAgentConfig> = Vec::new();

    // -- Phase A: Load non-subagent skills --

    for config in &settings.skills {
        match config {
            crate::config::SkillConfig::Builtin(cfg) => {
                skills.push(Box::new(builtin::BuiltinSkill::new_filtered(
                    settings.web_search.as_ref(),
                    &cfg.tools,
                )));
            }
            crate::config::SkillConfig::Mcp(cfg) => {
                let mut mcp_skill = mcp::McpSkill::new(*cfg.clone());
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
            crate::config::SkillConfig::Local(cfg) => {
                let path = std::path::Path::new(&cfg.path);
                match local::LocalSkill::from_dir(path) {
                    Ok(skill) => {
                        tracing::info!(
                            name = skill.name(),
                            path = cfg.path.as_str(),
                            "local skill loaded"
                        );
                        skills.push(Box::new(skill));
                    }
                    Err(e) => {
                        tracing::error!(
                            name = cfg.name.as_str(),
                            error = %e,
                            "local skill failed to load — skipping"
                        );
                    }
                }
            }
            crate::config::SkillConfig::Subagent(cfg) => {
                // Defer subagent construction to Phase B.
                subagent_configs.extend(cfg.agents.clone());
            }
        }
    }

    // Auto-discover skills from the workspace's skills/ directory.
    //
    // Skills are loaded in two phases:
    //   1. Collect name + description from each skill directory
    //   2. Build a compact <available_skills> list for the system prompt
    //
    // The full skill body is NOT loaded into the system prompt — it's
    // fetched on-demand via the load_skill tool.
    let mut skill_list: Vec<(String, String)> = Vec::new();

    if let Some(ws) = workspace {
        for dir in ws.skill_dirs() {
            match local::LocalSkill::from_dir(&dir) {
                Ok(skill) => {
                    tracing::info!(
                        name = skill.name(),
                        path = %dir.display(),
                        "workspace skill discovered"
                    );
                    skill_list.push((
                        skill.name().to_string(),
                        skill.skill_description().to_string(),
                    ));
                    skills.push(Box::new(skill));
                }
                Err(e) => {
                    tracing::error!(
                        path = %dir.display(),
                        error = %e,
                        "workspace skill failed to load — skipping"
                    );
                }
            }
        }
    }

    // Inject the <available_skills> list into the system prompt.
    if !skill_list.is_empty() {
        skills.push(Box::new(local::SkillListSkill::new(&skill_list)));
    }

    // -- Phase B: Construct subagent skill from loaded tools --
    //
    // Now that all non-subagent skills are loaded, flatten their tools
    // into a single list.  SubagentSkill will distribute these to each
    // subagent based on its `tools` filter config.
    //
    // Built-in subagents (planner, researcher) are always included.
    // User-defined subagents from dyson.json are appended after them,
    // so user configs can override built-ins by using the same name.
    let builtin_count = subagent::builtin_subagent_configs().len();
    let mut all_subagent_configs = subagent::builtin_subagent_configs();
    all_subagent_configs.extend(subagent_configs);

    {
        let parent_tools: Vec<std::sync::Arc<dyn crate::tool::Tool>> = skills
            .iter()
            .flat_map(|s| s.tools().iter().cloned())
            .collect();

        tracing::info!(
            builtin_subagents = builtin_count,
            user_subagents = all_subagent_configs.len() - builtin_count,
            parent_tools = parent_tools.len(),
            "constructing subagent skill"
        );

        let subagent_skill = subagent::SubagentSkill::new(
            &all_subagent_configs,
            settings,
            sandbox,
            workspace_arc,
            &parent_tools,
            registry,
        );

        if !subagent_skill.tools().is_empty() {
            skills.push(Box::new(subagent_skill));
        }
    }

    skills
}
