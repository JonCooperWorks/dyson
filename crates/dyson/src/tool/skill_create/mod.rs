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

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;

use crate::skill::local::{
    normalize_timeout_ms, validate_entrypoint, validate_skill_name, validate_slash_command,
};
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
         containing instructions that are listed and loaded on demand with load_skill. \
         Skills may also include optional script code and a slash command for direct chat execution. \
         Use this after solving a complex task to distill your approach into a reusable skill, \
         or to improve an existing skill based on experience. Skills auto-load after reload."
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
                },
                "slash_command": {
                    "type": "string",
                    "description": "Optional slash command for an executable hybrid skill, e.g. /review."
                },
                "execution": {
                    "type": "object",
                    "description": "Optional executable half of a hybrid skill. Omit or set kind=none for instruction-only skills.",
                    "properties": {
                        "kind": { "type": "string", "enum": ["none", "script"] },
                        "entrypoint": {
                            "type": "string",
                            "description": "Relative script path inside the skill directory, e.g. bin/run.sh."
                        },
                        "code": {
                            "type": "string",
                            "description": "Shell script source to write to the entrypoint. In improve mode, omitted code preserves existing files."
                        },
                        "timeout_ms": { "type": "integer", "minimum": 1 },
                        "input_schema": { "type": "object" }
                    }
                }
            },
            "required": ["name", "description", "instructions"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("skill_create")?;

        let name = input["name"].as_str().unwrap_or("").trim().to_string();
        let description = input["description"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let instructions = input["instructions"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let mode = input["mode"].as_str().unwrap_or("create");

        // Validate inputs.
        if name.is_empty() {
            return Ok(ToolOutput::error("'name' is required"));
        }
        if let Err(e) = validate_skill_name(&name) {
            return Ok(ToolOutput::error(e.sanitized_message()));
        }
        if description.is_empty() {
            return Ok(ToolOutput::error("'description' is required"));
        }
        if instructions.is_empty() {
            return Ok(ToolOutput::error("'instructions' is required"));
        }

        let file_key = format!("skills/{name}/SKILL.md");
        let metadata_key = format!("skills/{name}/dyson-skill.json");

        let mut ws = ws.write().await;

        let existing = ws.get(&file_key);
        let existing_metadata = ws
            .get(&metadata_key)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let execution = match parse_execution_request(input, mode, existing_metadata.as_ref()) {
            Ok(req) => req,
            Err(e) => return Ok(ToolOutput::error(e)),
        };
        if let Some(cmd) = execution.slash_command.as_deref()
            && let Err(e) = validate_slash_command(cmd)
        {
            return Ok(ToolOutput::error(e.sanitized_message()));
        }
        if let Some(entrypoint) = execution.entrypoint.as_deref()
            && let Err(e) = validate_entrypoint(entrypoint)
        {
            return Ok(ToolOutput::error(e.sanitized_message()));
        }
        if execution.kind == ExecutionKind::Script && execution.slash_command.is_none() {
            return Ok(ToolOutput::error(
                "script skills require a slash_command so users can execute them directly",
            ));
        }
        let script_key = execution
            .entrypoint
            .as_ref()
            .map(|entrypoint| format!("skills/{name}/{entrypoint}"));

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
                if let Some(script_key) = script_key.as_ref() {
                    let Some(code) = execution.code.as_ref() else {
                        return Ok(ToolOutput::error(
                            "script execution requires execution.code when creating a skill",
                        ));
                    };
                    ws.set(script_key, code);
                }
                ws.set(
                    &metadata_key,
                    &learned_metadata(&name, &description, &execution),
                );
                ws.save()?;

                // Journal the creation for memory.
                ws.journal(&format!("Created new skill '{name}': {description}"));
                ws.save()?;

                Ok(ToolOutput::success(format!(
                    "Created skill '{name}' at {file_key}. \
                     It will appear in the <available_skills> list after the next reload."
                )))
            }
            "update" => {
                let content = format_skill_md(&name, &description, &instructions);
                let verb = if existing.is_some() {
                    "Updated"
                } else {
                    "Created"
                };
                ws.set(&file_key, &content);
                if let Some(script_key) = script_key.as_ref() {
                    let Some(code) = execution.code.as_ref() else {
                        return Ok(ToolOutput::error(
                            "script execution requires execution.code when updating or creating a script skill",
                        ));
                    };
                    ws.set(script_key, code);
                }
                ws.set(
                    &metadata_key,
                    &learned_metadata(&name, &description, &execution),
                );
                ws.save()?;

                ws.journal(&format!("{verb} skill '{name}': {description}"));
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
                let improved = append_improvements(&existing_content, &description, &instructions);

                ws.set(&file_key, &improved);
                if let Some(script_key) = script_key.as_ref()
                    && let Some(code) = execution.code.as_ref()
                {
                    ws.set(script_key, code);
                }
                ws.set(
                    &metadata_key,
                    &learned_metadata(&name, &description, &execution),
                );
                ws.save()?;

                ws.journal(&format!("Improved skill '{name}': {description}"));
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

/// Format a complete SKILL.md file.
fn format_skill_md(name: &str, description: &str, instructions: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{instructions}\n")
}

#[cfg(test)]
fn is_valid_skill_name(name: &str) -> bool {
    validate_skill_name(name).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionKind {
    None,
    Script,
}

#[derive(Debug, Clone)]
struct ExecutionRequest {
    slash_command: Option<String>,
    kind: ExecutionKind,
    entrypoint: Option<String>,
    code: Option<String>,
    input_schema: Option<serde_json::Value>,
    timeout_ms: u64,
}

fn learned_metadata(name: &str, description: &str, execution: &ExecutionRequest) -> String {
    let mut metadata = json!({
        "schema_version": 2,
        "name": name,
        "version": "0.0.0-learned",
        "description": description,
        "origin": {
            "kind": "learned",
            "dream": "self-improvement",
        },
        "installed_at": now_unix_string(),
    });
    if let Some(cmd) = &execution.slash_command {
        metadata["slash_command"] = json!(cmd);
    }
    metadata["execution"] = match execution.kind {
        ExecutionKind::None => json!({ "kind": "none" }),
        ExecutionKind::Script => json!({
            "kind": "script",
            "entrypoint": execution.entrypoint.as_deref().unwrap_or("bin/run.sh"),
            "argument_mode": "raw",
            "input_schema": execution.input_schema.clone().unwrap_or_else(|| json!({
                "type": "object",
                "properties": {
                    "raw": { "type": "string" }
                }
            })),
            "timeout_ms": execution.timeout_ms,
        }),
    };
    format!(
        "{}\n",
        serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| "{}".into())
    )
}

fn parse_execution_request(
    input: &serde_json::Value,
    mode: &str,
    existing: Option<&serde_json::Value>,
) -> std::result::Result<ExecutionRequest, String> {
    let execution = input.get("execution").and_then(|v| v.as_object());
    let existing_execution = existing
        .and_then(|m| m.get("execution"))
        .and_then(|v| v.as_object());
    let inherit = mode == "improve";

    let slash_command = input
        .get("slash_command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            inherit.then(|| {
                existing
                    .and_then(|m| m.get("slash_command"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            })?
        });

    let kind_str = execution
        .and_then(|m| m.get("kind"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            inherit.then(|| {
                existing_execution
                    .and_then(|m| m.get("kind"))
                    .and_then(|v| v.as_str())
            })?
        })
        .unwrap_or("none");

    let kind = match kind_str {
        "none" => ExecutionKind::None,
        "script" => ExecutionKind::Script,
        other => return Err(format!("unknown execution.kind '{other}'")),
    };

    let entrypoint = execution
        .and_then(|m| m.get("entrypoint"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            inherit.then(|| {
                existing_execution
                    .and_then(|m| m.get("entrypoint"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            })?
        })
        .or_else(|| (kind == ExecutionKind::Script).then(|| "bin/run.sh".to_string()));

    let code = execution
        .and_then(|m| m.get("code"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let input_schema = execution
        .and_then(|m| m.get("input_schema"))
        .cloned()
        .or_else(|| {
            inherit.then(|| {
                existing_execution
                    .and_then(|m| m.get("input_schema"))
                    .cloned()
            })?
        });
    let timeout_ms = normalize_timeout_ms(
        execution
            .and_then(|m| m.get("timeout_ms"))
            .and_then(|v| v.as_u64())
            .or_else(|| {
                inherit.then(|| {
                    existing_execution
                        .and_then(|m| m.get("timeout_ms"))
                        .and_then(|v| v.as_u64())
                })?
            }),
    );

    Ok(ExecutionRequest {
        slash_command,
        kind,
        entrypoint,
        code,
        input_schema,
        timeout_ms,
    })
}

fn now_unix_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Append improvement notes to an existing SKILL.md.
///
/// Preserves the frontmatter structure but updates the description
/// and appends new instructions to the body.
fn append_improvements(existing: &str, new_description: &str, new_instructions: &str) -> String {
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
                writeln!(&mut new_frontmatter, "description: {new_description}").unwrap();
                found_desc = true;
            } else {
                new_frontmatter.push_str(line);
                new_frontmatter.push('\n');
            }
        }
        if !found_desc {
            writeln!(&mut new_frontmatter, "description: {new_description}").unwrap();
        }

        format!("---\n{new_frontmatter}---\n\n{body}\n\n## Improvements\n\n{new_instructions}\n")
    } else {
        // Malformed — infer frontmatter boundary and reconstruct properly.
        let mut fm_end = 0;
        for line in after_open.lines() {
            let t = line.trim();
            if t.is_empty() || !t.contains(':') {
                break;
            }
            fm_end += line.len() + 1;
        }
        if fm_end > 0 {
            let mut new_frontmatter = String::new();
            let mut found_desc = false;
            for line in after_open[..fm_end].trim_end().lines() {
                let line_trimmed = line.trim();
                if line_trimmed.starts_with("description:") {
                    writeln!(&mut new_frontmatter, "description: {new_description}").unwrap();
                    found_desc = true;
                } else {
                    new_frontmatter.push_str(line);
                    new_frontmatter.push('\n');
                }
            }
            if !found_desc {
                writeln!(&mut new_frontmatter, "description: {new_description}").unwrap();
            }
            let body = after_open[fm_end..].trim();
            format!(
                "---\n{new_frontmatter}---\n\n{body}\n\n## Improvements\n\n{new_instructions}\n"
            )
        } else {
            // Completely unrecoverable — wrap with new frontmatter.
            format!(
                "---\nname: unknown\ndescription: {new_description}\n---\n\n{existing}\n\n\
                 ## Improvements\n\n{new_instructions}\n"
            )
        }
    }
}

#[cfg(test)]
mod tests;
