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

/// A skill loaded from a SKILL.md file on disk.
///
/// Local skills contribute a system prompt fragment but no tools — they
/// guide the agent's behaviour through instructions rather than providing
/// new capabilities.
#[derive(Debug)]
pub struct LocalSkill {
    name: String,
    #[allow(dead_code)]
    description: String,
    system_prompt: String,
}

impl LocalSkill {
    /// Load a local skill from a file path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            DysonError::Config(format!(
                "failed to read skill file {}: {e}",
                path.display()
            ))
        })?;
        Self::parse(&content, path)
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
        let close_pos = after_open.find("\n---").ok_or_else(|| {
            DysonError::Config(format!(
                "skill file {display}: missing closing --- in frontmatter"
            ))
        })?;

        let frontmatter = &after_open[..close_pos];
        let body = after_open[close_pos + 4..].trim();

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
            system_prompt: body.to_string(),
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
        Some(&self.system_prompt)
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
            skill.system_prompt,
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
    fn parse_missing_closing_delimiter() {
        let content = "\
---
name: broken
";
        let err = LocalSkill::parse(content, &test_path()).unwrap_err();
        assert!(err.to_string().contains("missing closing ---"));
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
        assert!(err.to_string().contains("body (system prompt) must not be empty"));
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
        assert_eq!(skill.system_prompt, "Do something.");
    }

    #[test]
    fn from_file_loads_real_file() {
        let dir = std::env::temp_dir().join(format!(
            "dyson-skill-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test-skill.md");
        std::fs::write(
            &path,
            "---\nname: file-test\ndescription: loaded from disk\n---\n\nDo the thing.\n",
        )
        .unwrap();

        let skill = LocalSkill::from_file(&path).unwrap();
        assert_eq!(skill.name, "file-test");
        assert_eq!(skill.description, "loaded from disk");
        assert_eq!(skill.system_prompt, "Do the thing.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_file_errors_on_missing_file() {
        let err = LocalSkill::from_file(Path::new("/nonexistent/skill.md")).unwrap_err();
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
        assert_eq!(skill.system_prompt, "Body text.");
    }

    #[test]
    fn skill_trait_provides_system_prompt() {
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
        assert_eq!(skill.system_prompt(), Some("Custom instructions here."));
    }
}
