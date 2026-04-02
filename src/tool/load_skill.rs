// ===========================================================================
// LoadSkillTool — load a skill's full instructions on demand.
//
// LEARNING OVERVIEW
//
// Skills are discovered at startup and listed in <available_skills>, but
// their full body is NOT injected into the system prompt.  When the LLM
// decides a skill is relevant, it calls this tool to load the instructions.
//
// This two-phase approach (list at startup, load on demand) keeps the
// system prompt compact while still giving the agent access to detailed
// skill instructions when needed.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::skill::local::LocalSkill;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct LoadSkillTool;

#[async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn description(&self) -> &str {
        "Load the full instructions for a skill by name. Use this when you need to \
         apply a skill's detailed procedures. Check the <available_skills> list for \
         available skill names."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Name of the skill to load (from the <available_skills> list)."
                }
            },
            "required": ["skill_name"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace.as_ref().ok_or_else(|| {
            DysonError::tool("load_skill", "no workspace configured")
        })?;

        let skill_name = input["skill_name"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        if skill_name.is_empty() {
            return Ok(ToolOutput::error("'skill_name' is required"));
        }

        let ws = ws.read().await;

        // Read the skill's SKILL.md from the workspace.
        let skill_key = format!("skills/{skill_name}/SKILL.md");
        match ws.get(&skill_key) {
            Some(content) => {
                // Parse to extract the body (instructions without frontmatter).
                match LocalSkill::parse_body(&content) {
                    Some(body) => Ok(ToolOutput::success(body)),
                    None => {
                        // Fallback: return the raw content if parsing fails.
                        Ok(ToolOutput::success(content))
                    }
                }
            }
            None => {
                // Skill not found — list available skills helpfully.
                let available = ws.skill_dirs();
                let names: Vec<String> = available
                    .iter()
                    .filter_map(|p| {
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .map(String::from)
                    })
                    .collect();

                if names.is_empty() {
                    Ok(ToolOutput::error(format!(
                        "Skill '{skill_name}' not found. No skills are available."
                    )))
                } else {
                    Ok(ToolOutput::error(format!(
                        "Skill '{skill_name}' not found. Available skills: {}",
                        names.join(", ")
                    )))
                }
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_ctx(ws: InMemoryWorkspace) -> ToolContext {
        let workspace: Box<dyn crate::workspace::Workspace> = Box::new(ws);
        ToolContext {
            working_dir: std::env::temp_dir(),
            env: HashMap::new(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            workspace: Some(Arc::new(RwLock::new(workspace))),
            depth: 0,
        }
    }

    #[tokio::test]
    async fn loads_skill_body() {
        let ws = InMemoryWorkspace::new().with_file(
            "skills/code-review/SKILL.md",
            "---\nname: code-review\ndescription: Reviews code\n---\n\nStep 1: Read the code.\nStep 2: Find issues.",
        );
        let ctx = make_ctx(ws);
        let tool = LoadSkillTool;

        let result = tool
            .run(json!({"skill_name": "code-review"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("Step 1: Read the code."));
        assert!(result.content.contains("Step 2: Find issues."));
        // Should NOT contain frontmatter.
        assert!(!result.content.contains("---"));
    }

    #[tokio::test]
    async fn missing_skill_lists_available() {
        let ws = InMemoryWorkspace::new().with_file(
            "skills/deploy/SKILL.md",
            "---\nname: deploy\n---\n\nDeploy stuff.",
        );
        let ctx = make_ctx(ws);
        let tool = LoadSkillTool;

        let result = tool
            .run(json!({"skill_name": "nonexistent"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = LoadSkillTool;

        let result = tool
            .run(json!({"skill_name": ""}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("required"));
    }
}
