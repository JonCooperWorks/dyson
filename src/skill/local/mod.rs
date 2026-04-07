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
        if let Some(after_prefix) = trimmed.strip_prefix("---") {
            let after_open = &after_prefix.trim_start_matches(['\r', '\n']);
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
        if let Some(after_prefix) = trimmed.strip_prefix("---") {
            let after_open = &after_prefix.trim_start_matches(['\r', '\n']);

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
        if let Some((k, v)) = line.split_once(':')
            && k.trim() == key
        {
            return v.trim().to_string();
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

#[cfg(test)]
mod tests;
