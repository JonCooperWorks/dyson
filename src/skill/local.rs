// ===========================================================================
// Local skill — loads a SKILL.md file that defines a custom system prompt.
//
// SKILL.md format:
//
//   ---
//   name: code-review
//   description: Reviews code for quality and security issues
//   ---
//
//   You are a code review expert. When asked to review code:
//   1. Search the workspace for the relevant files
//   2. Analyze code quality, security, and patterns
//   3. Provide actionable feedback
//
// The YAML-like frontmatter between `---` delimiters provides metadata.
// Everything after the closing `---` is the system prompt injected into
// the agent's context.
// ===========================================================================

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::Tool;

// ---------------------------------------------------------------------------
// LocalSkill
// ---------------------------------------------------------------------------

/// A skill loaded from a `skills/<name>/SKILL.md` file on disk.
///
/// Local skills no longer inject their full body into the system prompt.
/// Instead, their name and description are collected into a compact
/// `<available_skills>` list, and the full instructions are loaded on
/// demand via the `load_skill` tool.
#[derive(Debug)]
pub struct LocalSkill {
    name: String,
    description: String,
    body: String,
}

impl LocalSkill {
    /// Load a local skill from a directory containing `SKILL.md`.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let skill_md = dir.join("SKILL.md");
        let content = std::fs::read_to_string(&skill_md).map_err(|e| {
            DysonError::Config(format!(
                "failed to read skill file {}: {e}",
                skill_md.display()
            ))
        })?;
        Self::parse(&content, &skill_md)
    }

    /// The skill's description (from frontmatter).
    pub fn skill_description(&self) -> &str {
        &self.description
    }

    /// The skill's full instruction body (the system prompt text).
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Extract just the body (instructions) from SKILL.md content.
    ///
    /// Returns `None` if parsing fails.  Used by `load_skill` to return
    /// instructions without frontmatter.
    pub fn parse_body(content: &str) -> Option<String> {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return None;
        }
        let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);
        let body = if let Some(close_pos) = after_open.find("\n---") {
            after_open[close_pos + 4..].trim()
        } else {
            // Fallback: infer frontmatter end from key: value lines.
            let mut fm_end = 0;
            for line in after_open.lines() {
                let t = line.trim();
                if t.is_empty() || !t.contains(':') {
                    break;
                }
                fm_end += line.len() + 1;
            }
            if fm_end == 0 {
                return None;
            }
            let fm_end = fm_end.min(after_open.len());
            after_open[fm_end..].trim()
        };
        if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        }
    }

    /// Parse SKILL.md content into a LocalSkill.
    ///
    /// Expects YAML-like frontmatter between `---` delimiters, followed by
    /// a body that becomes the system prompt.
    fn parse(content: &str, path: &Path) -> Result<Self> {
        let display = path.display();

        // Split on frontmatter delimiters.
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return Err(DysonError::Config(format!(
                "skill file {display}: missing frontmatter (must start with ---)"
            )));
        }

        // Find the closing ---
        let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);
        let (frontmatter, body) = if let Some(close_pos) = after_open.find("\n---") {
            let fm = &after_open[..close_pos];
            let b = after_open[close_pos + 4..].trim();
            (fm.to_string(), b.to_string())
        } else {
            // Auto-repair: infer frontmatter boundary by finding the first
            // line that isn't a `key: value` pair (empty line or body text).
            let mut fm_end = 0;
            for line in after_open.lines() {
                let t = line.trim();
                if t.is_empty() || !t.contains(':') {
                    break;
                }
                fm_end += line.len() + 1; // +1 for newline
            }
            if fm_end == 0 {
                return Err(DysonError::Config(format!(
                    "skill file {display}: missing closing --- in frontmatter"
                )));
            }
            // Clamp to string length — the last line may lack a trailing newline.
            let fm_end = fm_end.min(after_open.len());
            let fm = after_open[..fm_end].trim_end().to_string();
            let b = after_open[fm_end..].trim().to_string();

            // Write the repaired file back to disk (best-effort).
            let repaired = format!("---\n{fm}\n---\n\n{b}\n");
            if let Err(e) = std::fs::write(path, &repaired) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to write auto-repaired skill file",
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "auto-repaired malformed frontmatter (missing closing ---)",
                );
            }

            (fm, b)
        };

        // Parse frontmatter key-value pairs.
        let mut name: Option<String> = None;
        let mut description: Option<String> = None;

        for line in frontmatter.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "name" => name = Some(value.to_string()),
                    "description" => description = Some(value.to_string()),
                    _ => {} // ignore unknown keys
                }
            }
        }

        let name = name.ok_or_else(|| {
            DysonError::Config(format!(
                "skill file {display}: frontmatter missing required 'name' field"
            ))
        })?;

        if name.is_empty() {
            return Err(DysonError::Config(format!(
                "skill file {display}: 'name' field must not be empty"
            )));
        }

        if body.is_empty() {
            return Err(DysonError::Config(format!(
                "skill file {display}: body (system prompt) must not be empty"
            )));
        }

        Ok(Self {
            name,
            description: description.unwrap_or_default(),
            body: body.to_string(),
        })
    }
}

#[async_trait]
impl super::Skill for LocalSkill {
    fn name(&self) -> &str {
        &self.name
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &[]
    }

    fn system_prompt(&self) -> Option<&str> {
        // Full body is loaded on-demand via the load_skill tool.
        // The <available_skills> list is contributed by SkillListSkill.
        None
    }
}

// ---------------------------------------------------------------------------
// SkillListSkill — injects <available_skills> list into system prompt.
// ---------------------------------------------------------------------------

/// A lightweight skill that contributes the `<available_skills>` list to
/// the system prompt.  No tools — just a prompt fragment listing all
/// discovered skills by name and description.
pub struct SkillListSkill {
    prompt: String,
}

impl SkillListSkill {
    /// Build from a list of (name, description) pairs.
    pub fn new(skills: &[(String, String)]) -> Self {
        let prompt = if skills.is_empty() {
            String::new()
        } else {
            let mut lines = String::from("<available_skills>\n");
            for (name, desc) in skills {
                lines.push_str(&format!("- {name}: {desc}\n"));
            }
            lines.push_str(
                "</available_skills>\n\n\
                Use the load_skill tool to load a skill's full instructions before applying it.",
            );
            lines
        };
        Self { prompt }
    }
}

#[async_trait]
impl super::Skill for SkillListSkill {
    fn name(&self) -> &str {
        "skill-list"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &[]
    }

    fn system_prompt(&self) -> Option<&str> {
        if self.prompt.is_empty() {
            None
        } else {
            Some(&self.prompt)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_path() -> PathBuf {
        PathBuf::from("test-skill.md")
    }

    #[test]
    fn parse_valid_skill() {
        let content = "\
---
name: code-review
description: Reviews code for quality
---

You are a code review expert.
Analyze code quality and security.
";
        let skill = LocalSkill::parse(content, &test_path()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "Reviews code for quality");
        assert_eq!(
            skill.body,
            "You are a code review expert.\nAnalyze code quality and security."
        );
    }

    #[test]
    fn parse_missing_frontmatter() {
        let content = "Just a body with no frontmatter.";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(err.to_string().contains("missing frontmatter"));
    }

    #[test]
    fn parse_missing_closing_delimiter_no_body() {
        // Only frontmatter, no body at all — still an error after repair.
        let content = "\
---
name: broken
";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(
            err.to_string().contains("body (system prompt) must not be empty")
                || err.to_string().contains("missing closing ---")
        );
    }

    #[test]
    fn parse_auto_repairs_missing_closing_delimiter() {
        // Frontmatter without closing ---, followed by body text.
        let dir = std::env::temp_dir().join(format!("dyson-repair-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let skill_path = dir.join("SKILL.md");
        let content = "\
---
name: repaired
description: should auto-repair

Do the thing.
";
        std::fs::write(&skill_path, content).unwrap();

        let skill = LocalSkill::parse(content, &skill_path).unwrap();
        assert_eq!(skill.name, "repaired");
        assert_eq!(skill.description, "should auto-repair");
        assert_eq!(skill.body, "Do the thing.");

        // Verify the file was repaired on disk.
        let repaired = std::fs::read_to_string(&skill_path).unwrap();
        assert!(repaired.contains("\n---\n"), "repaired file should have closing ---");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_all_frontmatter_no_body_no_trailing_newline() {
        // Reproduces the panic: every line is key: value, no body, no
        // trailing newline — fm_end overshoots the string length.
        let content = "---\nname: markdown-pastebin\ndescription: Post markdown or plain text to markdownpastebin.com. Returns a shareable URL. No auth, no API key.";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        // Should error (empty body), not panic.
        assert!(err.to_string().contains("body (system prompt) must not be empty"));
    }

    #[test]
    fn parse_body_handles_missing_closing_delimiter() {
        let content = "\
---
name: no-close
description: test

Body text here.
";
        let body = LocalSkill::parse_body(content);
        assert_eq!(body, Some("Body text here.".to_string()));
    }

    #[test]
    fn parse_missing_name() {
        let content = "\
---
description: no name field
---

Some body.
";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(err.to_string().contains("missing required 'name'"));
    }

    #[test]
    fn parse_empty_body() {
        let content = "\
---
name: empty-body
---
";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("body (system prompt) must not be empty")
        );
    }

    #[test]
    fn parse_no_description_defaults_to_empty() {
        let content = "\
---
name: minimal
---

Do something.
";
        let skill = LocalSkill::parse(content, &test_path()).unwrap();
        assert_eq!(skill.name, "minimal");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, "Do something.");
    }

    #[test]
    fn from_dir_loads_skill() {
        let dir = std::env::temp_dir().join(format!("dyson-skill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: dir-test\ndescription: loaded from dir\n---\n\nDo the thing.\n",
        )
        .unwrap();

        let skill = LocalSkill::from_dir(&dir).unwrap();
        assert_eq!(skill.name, "dir-test");
        assert_eq!(skill.description, "loaded from dir");
        assert_eq!(skill.body, "Do the thing.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_dir_errors_on_missing_dir() {
        let err = LocalSkill::from_dir(Path::new("/nonexistent/skill")).unwrap_err();
        assert!(err.to_string().contains("failed to read skill file"));
    }

    #[test]
    fn parse_empty_name_rejected() {
        let content = "\
---
name:
---

Some body.
";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_unknown_frontmatter_keys_ignored() {
        let content = "\
---
name: flexible
description: has extra keys
version: 2
author: someone
---

Body text.
";
        let skill = LocalSkill::parse(content, &test_path()).unwrap();
        assert_eq!(skill.name, "flexible");
        assert_eq!(skill.body, "Body text.");
    }

    #[test]
    fn skill_trait_does_not_inject_system_prompt() {
        use crate::skill::Skill;

        let content = "\
---
name: prompt-test
---

Custom instructions here.
";
        let skill = LocalSkill::parse(content, &test_path()).unwrap();
        assert_eq!(skill.name(), "prompt-test");
        assert!(skill.tools().is_empty());
        // system_prompt returns None — full body loaded on-demand.
        assert_eq!(skill.system_prompt(), None);
        // But body() still returns the instructions.
        assert_eq!(skill.body(), "Custom instructions here.");
    }

    #[test]
    fn accessors_work() {
        let content = "\
---
name: test-skill
description: A test skill
---

Instructions here.
";
        let skill = LocalSkill::parse(content, &test_path()).unwrap();
        assert_eq!(skill.skill_description(), "A test skill");
        assert_eq!(skill.body(), "Instructions here.");
    }

    // -------------------------------------------------------------------
    // SkillListSkill tests
    // -------------------------------------------------------------------

    #[test]
    fn skill_list_empty_returns_none() {
        use crate::skill::Skill;
        let skill = SkillListSkill::new(&[]);
        assert!(skill.system_prompt().is_none());
    }

    #[test]
    fn skill_list_builds_prompt() {
        use crate::skill::Skill;
        let skills = vec![
            ("code-review".into(), "Reviews code".into()),
            ("deploy".into(), "Deploys things".into()),
        ];
        let skill = SkillListSkill::new(&skills);
        let prompt = skill.system_prompt().unwrap();
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("- code-review: Reviews code"));
        assert!(prompt.contains("- deploy: Deploys things"));
        assert!(prompt.contains("</available_skills>"));
        assert!(prompt.contains("load_skill"));
    }
}
