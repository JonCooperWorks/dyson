// ===========================================================================
// Local skill — loads a SKILL.md file that defines a custom system prompt.
//
// SKILL.md format (lenient):
//
// The parser accepts multiple formats, from most to least structured:
//
// 1. Full frontmatter (backward-compatible):
//
//   ---
//   name: code-review
//   description: Reviews code for quality and security issues
//   ---
//
//   You are a code review expert.
//
// 2. No frontmatter — name comes from the parent directory, first line
//    is the description, rest is the body:
//
//   Reviews code for quality and security issues
//
//   You are a code review expert.
//
// 3. No frontmatter, single block — name from directory, description
//    empty, entire file is the body:
//
//   You are a code review expert.
//   Analyze code quality, security, and patterns.
//
// The `name` field is always derived from the parent directory name
// (e.g., `skills/code-review/SKILL.md` → name = "code-review").
// Frontmatter `name:` is accepted but ignored in favor of the directory.
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
    ///
    /// The skill name is derived from the directory name, not the file
    /// content.  This means the directory must be named with a valid
    /// skill name (lowercase alphanumeric + hyphens).
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let skill_md = dir.join("SKILL.md");
        let content = std::fs::read_to_string(&skill_md).map_err(|e| {
            DysonError::Config(format!(
                "failed to read skill file {}: {e}",
                skill_md.display()
            ))
        })?;

        // Derive name from the directory (e.g., skills/code-review → "code-review").
        let dir_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        Self::parse(&content, &dir_name, &skill_md)
    }

    /// The skill's description (from frontmatter or first line).
    pub fn skill_description(&self) -> &str {
        &self.description
    }

    /// The skill's full instruction body (the system prompt text).
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Extract just the body (instructions) from SKILL.md content.
    ///
    /// Returns `None` if the content is empty.  Used by `load_skill` to
    /// return instructions without frontmatter.
    pub fn parse_body(content: &str) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }

        // If frontmatter is present, strip it.
        if trimmed.starts_with("---") {
            let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);
            if let Some(close_pos) = after_open.find("\n---") {
                let body = after_open[close_pos + 4..].trim();
                return if body.is_empty() { None } else { Some(body.to_string()) };
            }
            // Malformed frontmatter — fall through and treat entire content
            // as body (the name/description will just be part of the text).
        }

        // No frontmatter — return the whole thing.
        Some(trimmed.to_string())
    }

    /// Parse SKILL.md content into a LocalSkill.
    ///
    /// Accepts three formats:
    ///
    /// 1. **Frontmatter** — `---` delimiters with `description:` field,
    ///    body after closing `---`.  `name:` in frontmatter is ignored
    ///    (we always use `dir_name`).
    ///
    /// 2. **No frontmatter, multi-line** — first non-empty line is the
    ///    description, everything after the first blank line is the body.
    ///
    /// 3. **No frontmatter, single block** — entire content is the body,
    ///    description is empty.
    fn parse(content: &str, dir_name: &str, path: &Path) -> Result<Self> {
        let trimmed = content.trim();

        if trimmed.is_empty() {
            return Err(DysonError::Config(format!(
                "skill file {}: file is empty",
                path.display()
            )));
        }

        // --- Path 1: frontmatter present ---
        if trimmed.starts_with("---") {
            let after_open = &trimmed[3..].trim_start_matches(['\r', '\n']);

            if let Some(close_pos) = after_open.find("\n---") {
                // Well-formed frontmatter.
                let frontmatter = &after_open[..close_pos];
                let body = after_open[close_pos + 4..].trim();

                let description = extract_frontmatter_value(frontmatter, "description");

                if body.is_empty() {
                    return Err(DysonError::Config(format!(
                        "skill file {}: body (system prompt) must not be empty",
                        path.display()
                    )));
                }

                return Ok(Self {
                    name: dir_name.to_string(),
                    description,
                    body: body.to_string(),
                });
            }

            // Malformed frontmatter — try to extract description from what
            // looks like frontmatter, then treat the rest as body.
            let (description, body) = split_malformed_frontmatter(after_open);

            if body.is_empty() {
                return Err(DysonError::Config(format!(
                    "skill file {}: body (system prompt) must not be empty",
                    path.display()
                )));
            }

            return Ok(Self {
                name: dir_name.to_string(),
                description,
                body,
            });
        }

        // --- Path 2 & 3: no frontmatter ---
        let (description, body) = split_plain_content(trimmed);

        if body.is_empty() {
            // Single block — entire content is the body, no description.
            return Ok(Self {
                name: dir_name.to_string(),
                description: String::new(),
                body: trimmed.to_string(),
            });
        }

        Ok(Self {
            name: dir_name.to_string(),
            description,
            body,
        })
    }
}

/// Extract a value from frontmatter text for a given key.
fn extract_frontmatter_value(frontmatter: &str, key: &str) -> String {
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            if k.trim() == key {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Split malformed frontmatter (opened with `---` but never closed).
///
/// Scans lines looking for `key: value` pairs.  The first empty line or
/// the first line that doesn't start with a simple key (no spaces before
/// the colon) marks the end of the frontmatter region.  Returns
/// (description, body).
fn split_malformed_frontmatter(after_open: &str) -> (String, String) {
    let mut description = String::new();
    let mut fm_end = 0;

    for line in after_open.lines() {
        let t = line.trim();
        if t.is_empty() {
            fm_end += line.len() + 1;
            break;
        }
        // A frontmatter line looks like `key: value` where the key is a
        // simple identifier (no spaces).
        if let Some((key, value)) = t.split_once(':') {
            let key = key.trim();
            if key.contains(' ') || key.is_empty() {
                // Not a frontmatter key — this is body text.
                break;
            }
            if key == "description" {
                description = value.trim().to_string();
            }
            fm_end += line.len() + 1;
        } else {
            // Not a key: value line — start of body.
            break;
        }
    }

    let fm_end = fm_end.min(after_open.len());
    let body = after_open[fm_end..].trim().to_string();
    (description, body)
}

/// Split plain content (no frontmatter) into description + body.
///
/// First non-empty line is the description; everything after the first
/// blank line is the body.  If there's no blank line, body is empty
/// (caller should treat entire content as body).
fn split_plain_content(content: &str) -> (String, String) {
    let mut lines = content.lines();
    let first_line = match lines.next() {
        Some(l) => l.trim().to_string(),
        None => return (String::new(), String::new()),
    };

    // Find the first blank line.
    let mut rest_start = first_line.len() + 1; // +1 for newline
    let mut found_blank = false;
    for line in lines {
        if line.trim().is_empty() {
            rest_start += line.len() + 1;
            found_blank = true;
            break;
        }
        rest_start += line.len() + 1;
    }

    if !found_blank {
        return (first_line, String::new());
    }

    let rest_start = rest_start.min(content.len());
    let body = content[rest_start..].trim().to_string();
    (first_line, body)
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
        PathBuf::from("/fake/skills/test-skill/SKILL.md")
    }

    // -------------------------------------------------------------------
    // Frontmatter parsing (backward compat)
    // -------------------------------------------------------------------

    #[test]
    fn parse_valid_frontmatter() {
        let content = "\
---
name: ignored-because-dir-wins
description: Reviews code for quality
---

You are a code review expert.
Analyze code quality and security.
";
        let skill = LocalSkill::parse(content, "code-review", &test_path()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "Reviews code for quality");
        assert_eq!(
            skill.body,
            "You are a code review expert.\nAnalyze code quality and security."
        );
    }

    #[test]
    fn parse_frontmatter_name_from_dir() {
        let content = "\
---
name: old-name
description: test
---

Body.
";
        let skill = LocalSkill::parse(content, "new-name", &test_path()).unwrap();
        // Name always comes from directory, not frontmatter.
        assert_eq!(skill.name, "new-name");
    }

    #[test]
    fn parse_frontmatter_no_description() {
        let content = "\
---
name: minimal
---

Do something.
";
        let skill = LocalSkill::parse(content, "minimal", &test_path()).unwrap();
        assert_eq!(skill.name, "minimal");
        assert_eq!(skill.description, "");
        assert_eq!(skill.body, "Do something.");
    }

    #[test]
    fn parse_frontmatter_empty_body_rejected() {
        let content = "\
---
name: empty-body
---
";
        let err = LocalSkill::parse(content, "empty-body", &test_path()).unwrap_err();
        assert!(err.to_string().contains("body (system prompt) must not be empty"));
    }

    #[test]
    fn parse_frontmatter_unknown_keys_ignored() {
        let content = "\
---
name: flexible
description: has extra keys
version: 2
author: someone
---

Body text.
";
        let skill = LocalSkill::parse(content, "flexible", &test_path()).unwrap();
        assert_eq!(skill.name, "flexible");
        assert_eq!(skill.description, "has extra keys");
        assert_eq!(skill.body, "Body text.");
    }

    // -------------------------------------------------------------------
    // Malformed frontmatter (opened but not closed)
    // -------------------------------------------------------------------

    #[test]
    fn parse_malformed_frontmatter_with_body() {
        // Missing closing --- but has body after blank line.
        let content = "\
---
name: repaired
description: should still work

Do the thing.
";
        let skill = LocalSkill::parse(content, "repaired", &test_path()).unwrap();
        assert_eq!(skill.name, "repaired");
        assert_eq!(skill.description, "should still work");
        assert_eq!(skill.body, "Do the thing.");
    }

    #[test]
    fn parse_malformed_frontmatter_description_with_colons() {
        // The description itself contains colons — shouldn't confuse parser.
        let content = "\
---
name: markdown-pastebin
description: Post markdown to site.com. Returns a URL. No auth, no API key.

When asked to share text:
1. Format the content
2. Post to the pastebin
";
        let skill = LocalSkill::parse(content, "markdown-pastebin", &test_path()).unwrap();
        assert_eq!(skill.name, "markdown-pastebin");
        assert_eq!(
            skill.description,
            "Post markdown to site.com. Returns a URL. No auth, no API key."
        );
        assert!(skill.body.contains("When asked to share text:"));
    }

    #[test]
    fn parse_malformed_frontmatter_only_no_body() {
        // All frontmatter, no body — error.
        let content =
            "---\nname: pastebin\ndescription: Posts things. No auth, no API key.";
        let err = LocalSkill::parse(content, "pastebin", &test_path()).unwrap_err();
        assert!(err.to_string().contains("body (system prompt) must not be empty"));
    }

    // -------------------------------------------------------------------
    // No frontmatter (plain text)
    // -------------------------------------------------------------------

    #[test]
    fn parse_plain_text_with_description_and_body() {
        let content = "\
Reviews code for quality and security issues

You are a code review expert.
Analyze code quality, security, and patterns.
Provide actionable feedback.
";
        let skill = LocalSkill::parse(content, "code-review", &test_path()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "Reviews code for quality and security issues");
        assert!(skill.body.contains("You are a code review expert."));
    }

    #[test]
    fn parse_plain_text_single_block() {
        // No blank line separator — entire content is body, description empty.
        let content = "\
You are a code review expert.
Analyze code quality, security, and patterns.
";
        let skill = LocalSkill::parse(content, "code-review", &test_path()).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "");
        assert!(skill.body.contains("You are a code review expert."));
    }

    #[test]
    fn parse_empty_file_rejected() {
        let err = LocalSkill::parse("", "empty", &test_path()).unwrap_err();
        assert!(err.to_string().contains("file is empty"));
    }

    #[test]
    fn parse_whitespace_only_rejected() {
        let err = LocalSkill::parse("   \n\n  ", "blank", &test_path()).unwrap_err();
        assert!(err.to_string().contains("file is empty"));
    }

    // -------------------------------------------------------------------
    // from_dir
    // -------------------------------------------------------------------

    #[test]
    fn from_dir_loads_skill() {
        let dir = std::env::temp_dir().join(format!("dyson-skill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: old-name\ndescription: loaded from dir\n---\n\nDo the thing.\n",
        )
        .unwrap();

        let skill = LocalSkill::from_dir(&dir).unwrap();
        // Name comes from directory, not frontmatter.
        let expected_name = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(skill.name, expected_name);
        assert_eq!(skill.description, "loaded from dir");
        assert_eq!(skill.body, "Do the thing.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_dir_plain_text_skill() {
        let dir = std::env::temp_dir().join(format!("dyson-plain-skill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "Diagnose cowrie honeypot issues\n\nCheck logs and config for common problems.\n",
        )
        .unwrap();

        let skill = LocalSkill::from_dir(&dir).unwrap();
        let expected_name = dir.file_name().unwrap().to_str().unwrap();
        assert_eq!(skill.name, expected_name);
        assert_eq!(skill.description, "Diagnose cowrie honeypot issues");
        assert_eq!(skill.body, "Check logs and config for common problems.");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_dir_errors_on_missing_dir() {
        let err = LocalSkill::from_dir(Path::new("/nonexistent/skill")).unwrap_err();
        assert!(err.to_string().contains("failed to read skill file"));
    }

    // -------------------------------------------------------------------
    // parse_body
    // -------------------------------------------------------------------

    #[test]
    fn parse_body_with_frontmatter() {
        let content = "---\nname: test\ndescription: d\n---\n\nThe body.\n";
        assert_eq!(
            LocalSkill::parse_body(content),
            Some("The body.".to_string())
        );
    }

    #[test]
    fn parse_body_without_frontmatter() {
        let content = "Just plain instructions.\nDo the thing.";
        assert_eq!(
            LocalSkill::parse_body(content),
            Some("Just plain instructions.\nDo the thing.".to_string())
        );
    }

    #[test]
    fn parse_body_empty_returns_none() {
        assert_eq!(LocalSkill::parse_body(""), None);
        assert_eq!(LocalSkill::parse_body("   \n  "), None);
    }

    #[test]
    fn parse_body_malformed_frontmatter_returns_whole_content() {
        // Opened with --- but never closed — returns entire content.
        let content = "---\nname: test\ndescription: d\n\nThe body.\n";
        let body = LocalSkill::parse_body(content).unwrap();
        assert!(body.contains("The body.") || body.contains("name: test"));
    }

    // -------------------------------------------------------------------
    // Skill trait
    // -------------------------------------------------------------------

    #[test]
    fn skill_trait_does_not_inject_system_prompt() {
        use crate::skill::Skill;

        let content = "\
---
name: prompt-test
---

Custom instructions here.
";
        let skill = LocalSkill::parse(content, "prompt-test", &test_path()).unwrap();
        assert_eq!(skill.name(), "prompt-test");
        assert!(skill.tools().is_empty());
        assert_eq!(skill.system_prompt(), None);
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
        let skill = LocalSkill::parse(content, "test-skill", &test_path()).unwrap();
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
