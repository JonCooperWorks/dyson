// ===========================================================================
// SkillCreateTool — create or improve skills in the workspace.
//
// LEARNING OVERVIEW
//
// This is the self-improvement engine.  The agent can:
//
//   1. Create a new skill from experience (after solving a complex task,
//      distill the approach into a reusable SKILL.md file)
//   2. Improve an existing skill (refine instructions based on what
//      worked and what didn't)
//
// Skills are stored as SKILL.md files in the workspace's `skills/`
// directory.  They follow the same format as LocalSkill:
//
//   ---
//   name: code-review
//   description: Reviews code for quality and security
//   ---
//
//   When asked to review code:
//   1. Search for the relevant files
//   2. Analyze patterns and security issues
//   3. Provide actionable feedback
//
// On the next agent startup, these skills are auto-discovered by
// `create_skills()` and their system prompt fragments are injected
// into the agent's context.  The agent literally teaches itself new
// behaviors that persist across sessions.
//
// This is the Dyson equivalent of Hermes Agent's self-improving skills
// system — but implemented as a simple tool + workspace files rather
// than a separate subsystem.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct SkillCreateTool;

#[async_trait]
impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "skill_create"
    }

    fn description(&self) -> &str {
        "Create or update a skill in the workspace's skills/ directory. \
         Skills are SKILL.md files with YAML frontmatter (name, description) and a body \
         containing instructions that get injected into the system prompt on next startup. \
         Use this after solving a complex task to distill your approach into a reusable skill, \
         or to improve an existing skill based on experience. Skills auto-load on next startup."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (lowercase, hyphens ok). Used as the filename: skills/<name>.md"
                },
                "description": {
                    "type": "string",
                    "description": "Short description of what this skill does (one line)."
                },
                "instructions": {
                    "type": "string",
                    "description": "The skill's system prompt instructions. This is the body of the SKILL.md \
                                    file — it tells the agent how to behave when this skill is relevant. \
                                    Be specific: include step-by-step procedures, tool usage patterns, \
                                    quality checks, and common pitfalls."
                },
                "mode": {
                    "type": "string",
                    "enum": ["create", "update", "improve"],
                    "description": "'create': create new skill (fails if exists). \
                                    'update': overwrite existing skill. \
                                    'improve': read existing skill, append improvement notes. \
                                    Defaults to 'create'."
                }
            },
            "required": ["name", "description", "instructions"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace.as_ref().ok_or_else(|| {
            DysonError::tool("skill_create", "no workspace configured")
        })?;

        let name = input["name"].as_str().unwrap_or("").trim().to_string();
        let description = input["description"].as_str().unwrap_or("").trim().to_string();
        let instructions = input["instructions"].as_str().unwrap_or("").trim().to_string();
        let mode = input["mode"].as_str().unwrap_or("create");

        // Validate inputs.
        if name.is_empty() {
            return Ok(ToolOutput::error("'name' is required"));
        }
        if !is_valid_skill_name(&name) {
            return Ok(ToolOutput::error(
                "Invalid skill name. Use lowercase letters, numbers, and hyphens only \
                 (e.g., 'code-review', 'deploy-helper').",
            ));
        }
        if description.is_empty() {
            return Ok(ToolOutput::error("'description' is required"));
        }
        if instructions.is_empty() {
            return Ok(ToolOutput::error("'instructions' is required"));
        }

        let file_key = format!("skills/{name}/SKILL.md");

        let mut ws = ws.write().await;

        let existing = ws.get(&file_key);

        match mode {
            "create" => {
                if existing.is_some() {
                    return Ok(ToolOutput::error(format!(
                        "Skill '{name}' already exists. Use mode 'update' to overwrite \
                         or 'improve' to append improvements."
                    )));
                }
                let content = format_skill_md(&name, &description, &instructions);
                ws.set(&file_key, &content);
                ws.save()?;

                // Journal the creation for memory.
                ws.journal(&format!(
                    "Created new skill '{name}': {description}"
                ));
                ws.save()?;

                Ok(ToolOutput::success(format!(
                    "Created skill '{name}' at {file_key}. \
                     It will appear in the <available_skills> list after the next reload."
                )))
            }
            "update" => {
                let content = format_skill_md(&name, &description, &instructions);
                let verb = if existing.is_some() { "Updated" } else { "Created" };
                ws.set(&file_key, &content);
                ws.save()?;

                ws.journal(&format!(
                    "{verb} skill '{name}': {description}"
                ));
                ws.save()?;

                Ok(ToolOutput::success(format!(
                    "{verb} skill '{name}' at {file_key}. \
                     Changes take effect after the next reload."
                )))
            }
            "improve" => {
                let existing_content = match existing {
                    Some(content) => content,
                    None => {
                        return Ok(ToolOutput::error(format!(
                            "Skill '{name}' does not exist. Use mode 'create' to create it first."
                        )));
                    }
                };

                // Parse existing skill to preserve structure, then append
                // improvement notes to the body.
                let improved = append_improvements(
                    &existing_content,
                    &description,
                    &instructions,
                );

                ws.set(&file_key, &improved);
                ws.save()?;

                ws.journal(&format!(
                    "Improved skill '{name}': {description}"
                ));
                ws.save()?;

                Ok(ToolOutput::success(format!(
                    "Improved skill '{name}'. Appended new instructions and updated description. \
                     Changes take effect after the next reload."
                )))
            }
            other => Ok(ToolOutput::error(format!(
                "Unknown mode '{other}'. Use 'create', 'update', or 'improve'."
            ))),
        }
    }
}

/// Validate a skill name: lowercase alphanumeric + hyphens, no spaces.
fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Format a complete SKILL.md file.
fn format_skill_md(name: &str, description: &str, instructions: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n{instructions}\n"
    )
}

/// Append improvement notes to an existing SKILL.md.
///
/// Preserves the frontmatter structure but updates the description
/// and appends new instructions to the body.
fn append_improvements(
    existing: &str,
    new_description: &str,
    new_instructions: &str,
) -> String {
    // Try to parse the existing structure.
    let trimmed = existing.trim_start();
    if !trimmed.starts_with("---") {
        // Not a valid SKILL.md — just prepend frontmatter.
        return format!(
            "---\nname: unknown\ndescription: {new_description}\n---\n\n{existing}\n\n\
             ## Improvements\n\n{new_instructions}\n"
        );
    }

    let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);
    if let Some(close_pos) = after_open.find("\n---") {
        let frontmatter = &after_open[..close_pos];
        let body = after_open[close_pos + 4..].trim();

        // Update description in frontmatter.
        let mut new_frontmatter = String::new();
        let mut found_desc = false;
        for line in frontmatter.lines() {
            let line_trimmed = line.trim();
            if line_trimmed.starts_with("description:") {
                new_frontmatter.push_str(&format!("description: {new_description}\n"));
                found_desc = true;
            } else {
                new_frontmatter.push_str(line);
                new_frontmatter.push('\n');
            }
        }
        if !found_desc {
            new_frontmatter.push_str(&format!("description: {new_description}\n"));
        }

        format!(
            "---\n{new_frontmatter}---\n\n{body}\n\n## Improvements\n\n{new_instructions}\n"
        )
    } else {
        // Malformed — just append.
        format!("{existing}\n\n## Improvements\n\n{new_instructions}\n")
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

    #[test]
    fn valid_skill_names() {
        assert!(is_valid_skill_name("code-review"));
        assert!(is_valid_skill_name("deploy"));
        assert!(is_valid_skill_name("my-skill-2"));
        assert!(is_valid_skill_name("a"));
    }

    #[test]
    fn invalid_skill_names() {
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("Code-Review")); // uppercase
        assert!(!is_valid_skill_name("my skill"));     // space
        assert!(!is_valid_skill_name("-leading"));      // leading hyphen
        assert!(!is_valid_skill_name("trailing-"));     // trailing hyphen
        assert!(!is_valid_skill_name("my_skill"));      // underscore
        assert!(!is_valid_skill_name("../escape"));     // path traversal
    }

    #[test]
    fn format_skill_md_produces_valid_skill() {
        use crate::skill::Skill;

        let content = format_skill_md("test", "A test skill", "Do the thing.");
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: test"));
        assert!(content.contains("description: A test skill"));
        assert!(content.contains("Do the thing."));

        // Verify it parses as a valid LocalSkill.
        let skill = crate::skill::local::LocalSkill::from_dir(
            // Write to temp dir for parsing.
            &{
                let dir = std::env::temp_dir().join("dyson-test-skill-dir");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("SKILL.md"), &content).unwrap();
                dir
            },
        )
        .unwrap();
        assert_eq!(skill.name(), "test");
    }

    #[test]
    fn append_improvements_updates_description() {
        let existing = "---\nname: deploy\ndescription: Old desc\n---\n\nOriginal instructions.\n";
        let improved = append_improvements(existing, "New desc", "New step.");

        assert!(improved.contains("description: New desc"));
        assert!(improved.contains("Original instructions."));
        assert!(improved.contains("## Improvements"));
        assert!(improved.contains("New step."));
    }

    #[tokio::test]
    async fn create_skill() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "code-review",
                    "description": "Reviews code for quality",
                    "instructions": "1. Read the code\n2. Check for issues\n3. Report findings"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("Created skill 'code-review'"));

        // Verify it was written to the workspace.
        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        let content = ws.get("skills/code-review/SKILL.md").unwrap();
        assert!(content.contains("name: code-review"));
        assert!(content.contains("Read the code"));
    }

    #[tokio::test]
    async fn create_duplicate_fails() {
        let ws = InMemoryWorkspace::new()
            .with_file("skills/existing/SKILL.md", "---\nname: existing\ndescription: x\n---\n\nBody.");
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "existing",
                    "description": "New desc",
                    "instructions": "New body"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("already exists"));
    }

    #[tokio::test]
    async fn update_overwrites() {
        let ws = InMemoryWorkspace::new()
            .with_file("skills/deploy/SKILL.md", "---\nname: deploy\ndescription: old\n---\n\nOld.");
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "deploy",
                    "description": "New deploy skill",
                    "instructions": "New deploy instructions",
                    "mode": "update"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Updated skill 'deploy'"));

        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        let content = ws.get("skills/deploy/SKILL.md").unwrap();
        assert!(content.contains("New deploy instructions"));
        assert!(!content.contains("Old."));
    }

    #[tokio::test]
    async fn improve_appends() {
        let ws = InMemoryWorkspace::new()
            .with_file(
                "skills/review/SKILL.md",
                "---\nname: review\ndescription: Reviews code\n---\n\nOriginal approach.",
            );
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "review",
                    "description": "Reviews code with security focus",
                    "instructions": "Also check for SQL injection and XSS.",
                    "mode": "improve"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Improved skill 'review'"));

        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        let content = ws.get("skills/review/SKILL.md").unwrap();
        assert!(content.contains("Original approach."));
        assert!(content.contains("## Improvements"));
        assert!(content.contains("SQL injection"));
        assert!(content.contains("security focus"));
    }

    #[tokio::test]
    async fn improve_nonexistent_fails() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "ghost",
                    "description": "desc",
                    "instructions": "inst",
                    "mode": "improve"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("does not exist"));
    }

    #[tokio::test]
    async fn invalid_name_rejected() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        let result = tool
            .run(
                json!({
                    "name": "Bad Name!",
                    "description": "desc",
                    "instructions": "inst"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Invalid skill name"));
    }

    #[tokio::test]
    async fn journals_creation() {
        let ws = InMemoryWorkspace::new();
        let ctx = make_ctx(ws);
        let tool = SkillCreateTool;

        tool.run(
            json!({
                "name": "logged",
                "description": "test logging",
                "instructions": "body"
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Check that a journal entry was created.
        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        let files = ws.list_files();
        let journal = files.iter().find(|f| f.starts_with("memory/"));
        assert!(journal.is_some(), "Expected a journal entry to be created");
    }
}
