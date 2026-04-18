// ===========================================================================
// BuiltinSkill — Dyson's built-in tool suite.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Wraps Dyson's built-in tools (bash, and future read_file/write_file/
//   edit_file) into a Skill implementation.  This is the default skill
//   that's always loaded unless explicitly disabled.
//
// Why wrap tools in a skill?
//   The agent loop only interacts with skills — it doesn't know about
//   individual tools directly.  By wrapping builtins in a Skill, they
//   plug into the same lifecycle (on_load, on_unload, after_tool) and
//   discovery mechanism as MCP skills and local skills.  No special-casing.
//
// Adding new built-in tools:
//   1. Create the tool in tool/ (e.g., tool/read_file.rs)
//   2. Add it to the `tools` vec in BuiltinSkill::new()
//   3. Done — the agent discovers it automatically via the Skill trait
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;

use crate::skill::Skill;
use crate::tool::Tool;
use crate::tool::bash::BashTool;
use crate::tool::bulk_edit::BulkEditTool;
use crate::tool::dependency_scan::DependencyScanTool;
use crate::tool::edit_file::EditFileTool;
use crate::tool::kb_search::KbSearchTool;
use crate::tool::kb_status::KbStatusTool;
use crate::tool::list_files::ListFilesTool;
use crate::tool::load_skill::LoadSkillTool;
use crate::tool::memory_search::MemorySearchTool;
use crate::tool::read_file::ReadFileTool;
use crate::tool::search_files::SearchFilesTool;
use crate::tool::send_file::SendFileTool;
use crate::tool::swarm_checkpoint::SwarmCheckpointTool;
use crate::tool::image_generate;
use crate::tool::web_fetch::WebFetchTool;
use crate::tool::web_search;
use crate::tool::workspace_search::WorkspaceSearchTool;
use crate::tool::workspace_update::WorkspaceUpdateTool;
use crate::tool::workspace_view::WorkspaceViewTool;
use crate::tool::write_file::WriteFileTool;
use crate::tool::security::AstDescribeTool;
use crate::tool::security::AstQueryTool;
use crate::tool::security::AttackSurfaceAnalyzerTool;
use crate::tool::security::ExploitBuilderTool;
use crate::tool::security::TaintTraceTool;

// ---------------------------------------------------------------------------
// BuiltinSkill
// ---------------------------------------------------------------------------

/// Skill that provides Dyson's built-in tools.
///
/// Currently provides:
/// - **bash**: Shell command execution
/// - **workspace_view**: View/list workspace files (SOUL.md, MEMORY.md, etc.)
/// - **workspace_search**: Search across workspace files
/// - **workspace_update**: Update workspace files (set or append content)
///
/// The workspace tools give the agent runtime access to its identity and
/// memory files through the `Workspace` trait, enabling it to read and
/// evolve its own personality, memory, and journals.
///
/// The system prompt fragment describes what tools are available and gives
/// the LLM usage guidance.
pub struct BuiltinSkill {
    /// The built-in tools, stored as Arc for shared ownership with the agent.
    tools: Vec<Arc<dyn Tool>>,

    /// System prompt describing the available tools.
    ///
    /// Generated at construction time based on which tools are enabled.
    system_prompt: String,
}

impl BuiltinSkill {
    /// Create a new BuiltinSkill with all default tools, optionally
    /// filtered to only the named tools.
    ///
    /// When `filter` is non-empty, only tools whose names appear in the
    /// list are included.  When empty, all tools are included.
    ///
    /// When `web_search_config` is `Some`, the `web_search` tool is
    /// registered with the configured search provider.  When `None`,
    /// the tool is simply absent.
    pub fn new_filtered(
        web_search_config: Option<&crate::config::WebSearchConfig>,
        image_provider_config: Option<&crate::config::ProviderConfig>,
        image_model_override: Option<&str>,
        filter: &[String],
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(BashTool::default()),
            Arc::new(ReadFileTool),
            Arc::new(WriteFileTool),
            Arc::new(EditFileTool),
            Arc::new(BulkEditTool),
            Arc::new(ListFilesTool),
            Arc::new(SearchFilesTool),
            Arc::new(SendFileTool),
            Arc::new(MemorySearchTool),
            Arc::new(WorkspaceViewTool),
            Arc::new(WorkspaceSearchTool),
            Arc::new(WorkspaceUpdateTool),
            Arc::new(LoadSkillTool),
            Arc::new(KbSearchTool),
            Arc::new(KbStatusTool),
            Arc::new(SwarmCheckpointTool),
            Arc::new(WebFetchTool::default()),
            Arc::new(AstDescribeTool),
            Arc::new(AstQueryTool),
            Arc::new(AttackSurfaceAnalyzerTool),
            Arc::new(ExploitBuilderTool),
            Arc::new(TaintTraceTool),
            Arc::new(DependencyScanTool),
        ];

        if let Some(ws_cfg) = web_search_config {
            match web_search::create_provider(ws_cfg) {
                Ok(provider) => {
                    tools.push(Arc::new(web_search::WebSearchTool::new(provider)));
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to create search provider — skipping web_search tool"
                    );
                }
            }
        }

        if let Some(ig_cfg) = image_provider_config {
            match image_generate::create_provider(ig_cfg, image_model_override) {
                Ok(provider) => {
                    tools.push(Arc::new(image_generate::ImageGenerateTool::new(provider)));
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to create image generation provider — skipping image_generate tool"
                    );
                }
            }
        }

        // Apply tool filter if specified.
        if !filter.is_empty() {
            tools.retain(|t| filter.iter().any(|name| name == t.name()));
        }

        // Build the system prompt dynamically from the loaded tools.
        let tool_list: Vec<String> = tools
            .iter()
            .map(|t| format!("- **{}**: {}", t.name(), t.description()))
            .collect();

        let system_prompt = format!(
            "You have access to the following built-in tools:\n\n{}\n\n\
             Use these tools to help answer questions and complete tasks. \
             When running commands, prefer concise output. \
             Check command results before proceeding to the next step.",
            tool_list.join("\n")
        );

        Self {
            tools,
            system_prompt,
        }
    }
}

impl BuiltinSkill {
    /// Create a new BuiltinSkill with all default tools (no filter).
    pub fn new(
        web_search_config: Option<&crate::config::WebSearchConfig>,
        image_provider_config: Option<&crate::config::ProviderConfig>,
        image_model_override: Option<&str>,
    ) -> Self {
        Self::new_filtered(web_search_config, image_provider_config, image_model_override, &[])
    }
}

impl Default for BuiltinSkill {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

// ---------------------------------------------------------------------------
// Skill implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Skill for BuiltinSkill {
    fn name(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(&self.system_prompt)
    }

    /// Inject ephemeral context before each LLM turn.
    ///
    /// Currently injects:
    /// - Current date and time in UTC (models have no clock otherwise)
    ///
    /// This runs on every turn, so keep it lightweight — no I/O, no blocking.
    async fn before_turn(&self) -> crate::Result<Option<String>> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (y, m, d) = crate::util::unix_to_ymd(secs);
        let day_secs = secs % 86400;
        let hour = day_secs / 3600;
        let minute = (day_secs % 3600) / 60;

        let fragment = format!(
            "[Current time: {y:04}-{m:02}-{d:02} {hour:02}:{minute:02} UTC]",
        );
        Ok(Some(fragment))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_builtin_tools() {
        let skill = BuiltinSkill::new(None, None, None);
        let tools = skill.tools();
        assert_eq!(tools.len(), 23);
        assert_eq!(tools[0].name(), "bash");
        assert_eq!(tools[1].name(), "read_file");
        assert_eq!(tools[2].name(), "write_file");
        assert_eq!(tools[3].name(), "edit_file");
        assert_eq!(tools[4].name(), "bulk_edit");
        assert_eq!(tools[5].name(), "list_files");
        assert_eq!(tools[6].name(), "search_files");
        assert_eq!(tools[7].name(), "send_file");
        assert_eq!(tools[8].name(), "memory_search");
        assert_eq!(tools[9].name(), "workspace_view");
        assert_eq!(tools[10].name(), "workspace_search");
        assert_eq!(tools[11].name(), "workspace_update");
        assert_eq!(tools[12].name(), "load_skill");
        assert_eq!(tools[13].name(), "kb_search");
        assert_eq!(tools[14].name(), "kb_status");
        assert_eq!(tools[15].name(), "swarm_checkpoint");
        assert_eq!(tools[16].name(), "web_fetch");
        assert_eq!(tools[17].name(), "ast_describe");
        assert_eq!(tools[18].name(), "ast_query");
        assert_eq!(tools[19].name(), "attack_surface_analyzer");
        assert_eq!(tools[20].name(), "exploit_builder");
        assert_eq!(tools[21].name(), "taint_trace");
        assert_eq!(tools[22].name(), "dependency_scan");
    }

    #[test]
    fn filter_restricts_tools() {
        let skill = BuiltinSkill::new_filtered(
            None,
            None,
            None,
            &["bash".to_string(), "read_file".to_string()],
        );
        let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["bash", "read_file"]);
    }

    #[test]
    fn empty_filter_includes_all() {
        let all = BuiltinSkill::new(None, None, None);
        let filtered = BuiltinSkill::new_filtered(None, None, None, &[]);
        assert_eq!(all.tools().len(), filtered.tools().len());
    }

    #[test]
    fn has_system_prompt() {
        let skill = BuiltinSkill::new(None, None, None);
        let prompt = skill.system_prompt().unwrap();
        assert!(prompt.contains("bash"));
        assert!(!prompt.is_empty());
    }

    #[test]
    fn skill_name() {
        let skill = BuiltinSkill::new(None, None, None);
        assert_eq!(skill.name(), "builtin");
    }

    #[test]
    fn image_generate_tool_registered_when_configured() {
        let config = crate::config::ProviderConfig {
            provider_type: crate::config::LlmProvider::Gemini,
            models: vec!["gemini-3-pro-image-preview".into()],
            api_key: crate::auth::Credential::new("test-key".into()),
            base_url: None,
        };
        let skill = BuiltinSkill::new(None, Some(&config), None);
        let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
        assert!(names.contains(&"image_generate"));
        assert_eq!(skill.tools().len(), 24);
    }

    #[test]
    fn image_model_override_respected() {
        let config = crate::config::ProviderConfig {
            provider_type: crate::config::LlmProvider::Gemini,
            models: vec!["gemini-2.5-flash".into()],
            api_key: crate::auth::Credential::new("test-key".into()),
            base_url: None,
        };
        // With override, should still register the tool successfully.
        let skill = BuiltinSkill::new(None, Some(&config), Some("gemini-3-pro-image-preview"));
        let names: Vec<&str> = skill.tools().iter().map(|t| t.name()).collect();
        assert!(names.contains(&"image_generate"));
    }

    #[tokio::test]
    async fn before_turn_injects_datetime() {
        let skill = BuiltinSkill::new(None, None, None);
        let fragment = skill.before_turn().await.unwrap();
        assert!(fragment.is_some(), "before_turn should return Some");
        let text = fragment.unwrap();
        assert!(text.starts_with("[Current time: "), "got: {text}");
        assert!(text.ends_with(" UTC]"), "got: {text}");
        // Should contain a date-like pattern: YYYY-MM-DD HH:MM
        assert!(text.len() > 25, "too short: {text}");
    }
}
