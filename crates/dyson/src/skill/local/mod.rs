// ===========================================================================
// Local skill — loads a SKILL.md file that defines a custom system prompt.
//
// SKILL.md format (freeform):
//
// Skills are freeform text. The parser NEVER rejects a non-empty file — it
// only tries to extract a short description for the `<available_skills>`
// list, then uses the remaining content as the body. If extraction fails
// for any reason, the whole file becomes the body. The only failure case
// is a completely empty file.
//
// The parser recognises three common shapes (from most to least structured),
// but any of them may be partial or malformed and still load:
//
// 1. YAML-style frontmatter:
//
//   ---
//   name: code-review
//   description: Reviews code for quality and security issues
//   ---
//
//   You are a code review expert.
//
// 2. No frontmatter, description-first: the first line is the description,
//    everything after the first blank line is the body:
//
//   Reviews code for quality and security issues
//
//   You are a code review expert.
//
// 3. No frontmatter, single block — entire file is the body, no description:
//
//   You are a code review expert.
//   Analyze code quality, security, and patterns.
//
// Robustness rules:
//
// - The `name` field is always derived from the parent directory name
//   (e.g., `skills/code-review/SKILL.md` → name = "code-review").
//   Frontmatter `name:` is accepted but ignored in favor of the directory.
// - A leading `---` is only treated as frontmatter if it's followed by a
//   newline. Markdown documents that begin with a horizontal rule or a
//   line like `---foo` are NOT mis-parsed.
// - Frontmatter that lacks a closing `---` is still accepted — we salvage
//   whatever `key: value` lines look well-formed and treat the rest as
//   body text.
// - If the "body" extraction produces an empty string (e.g., the file is
//   frontmatter-only, or the split heuristic found nothing), the entire
//   file content is used as the body so the skill is still loadable.
// ===========================================================================

use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{DysonError, Result};
use crate::tool::Tool;

/// Maximum allowed SKILL.md content size (64 KB by default).
///
/// Prevents excessively large skill files from bloating the system prompt
/// or being used as a vector for prompt injection via sheer volume.
const MAX_SKILL_CONTENT_SIZE: usize = 64 * 1024;

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

        validate_skill_content(&content, &skill_md)?;

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
    /// Returns `None` only if the content is empty or whitespace-only.
    /// For any other input the function succeeds — if frontmatter is
    /// present and well-formed, it's stripped; otherwise the whole
    /// content is returned verbatim.  Used by `load_skill` to return
    /// instructions without frontmatter noise.
    pub fn parse_body(content: &str) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Only treat a leading `---` as frontmatter if it's followed by a
        // newline — otherwise it's likely a markdown horizontal rule or
        // part of the body text and should be preserved.
        let looks_like_frontmatter = trimmed
            .strip_prefix("---")
            .map(|rest| rest.starts_with(['\r', '\n']))
            .unwrap_or(false);

        if looks_like_frontmatter {
            let after_prefix = trimmed.strip_prefix("---").unwrap();
            let after_open = after_prefix.trim_start_matches(['\r', '\n']);
            if let Some((_, body_start)) = find_frontmatter_close(after_open) {
                let body = after_open[body_start..].trim();
                if !body.is_empty() {
                    return Some(body.to_string());
                }
                // Frontmatter-only file — fall through to returning the
                // whole file so the caller still gets something usable.
            }
        }

        // No usable frontmatter split — return the whole thing.
        Some(trimmed.to_string())
    }

    /// Parse SKILL.md content into a LocalSkill.
    ///
    /// Skills are freeform text.  This function never rejects a non-empty
    /// file.  It tries to extract a short description for the skill list
    /// and splits out a body; if any of that fails it falls back to using
    /// the entire file as the body.  The only failure case is an empty /
    /// whitespace-only file, which genuinely has nothing to load.
    fn parse(content: &str, dir_name: &str, path: &Path) -> Result<Self> {
        let trimmed = content.trim();

        if trimmed.is_empty() {
            return Err(DysonError::Config(format!(
                "skill file {}: file is empty",
                path.display()
            )));
        }

        let (description, body) = extract_description_and_body(trimmed);

        Ok(Self {
            name: dir_name.to_string(),
            description,
            body,
        })
    }
}

/// Extract `(description, body)` from a non-empty, already-trimmed skill file.
///
/// This is infallible: it always returns a non-empty body.  If the heuristics
/// below can't find sensible structure, the entire file content is returned
/// as the body with an empty description.
fn extract_description_and_body(trimmed: &str) -> (String, String) {
    // A leading `---` is only treated as frontmatter if it's followed by a
    // newline.  This keeps us from mis-parsing markdown that happens to
    // start with a horizontal rule or a line like `---foo`.
    let looks_like_frontmatter = trimmed
        .strip_prefix("---")
        .map(|rest| rest.starts_with(['\r', '\n']))
        .unwrap_or(false);

    if looks_like_frontmatter {
        // Unwrap is safe: we just confirmed the prefix above.
        let after_prefix = trimmed.strip_prefix("---").unwrap();
        let after_open = after_prefix.trim_start_matches(['\r', '\n']);

        if let Some(close_pos) = find_frontmatter_close(after_open) {
            // Well-formed frontmatter.
            let frontmatter = &after_open[..close_pos.0];
            let body = after_open[close_pos.1..].trim();
            let description = extract_frontmatter_value(frontmatter, "description");

            // If the body is empty, fall back to the full file so the skill
            // is still usable — freeform skills may legitimately consist of
            // just metadata, or the author may rely on the description alone.
            let body = if body.is_empty() {
                trimmed.to_string()
            } else {
                body.to_string()
            };
            return (description, body);
        }

        // Malformed frontmatter — try to salvage description lines, then
        // use whatever follows as the body.  If we can't split out a body,
        // fall back to the full file.
        let (description, body) = split_malformed_frontmatter(after_open);
        let body = if body.is_empty() {
            trimmed.to_string()
        } else {
            body
        };
        return (description, body);
    }

    // No frontmatter — try the "first line is description, blank line,
    // body" split.  If there's no blank line, treat the whole file as body.
    let (description, body) = split_plain_content(trimmed);
    if body.is_empty() {
        return (String::new(), trimmed.to_string());
    }
    (description, body)
}

/// Locate the closing `---` of a YAML-style frontmatter block.
///
/// Returns `Some((body_split_start, body_start))` where `body_split_start`
/// is the end of the frontmatter text (exclusive of the newline before
/// `---`) and `body_start` is the index where body content begins.
///
/// The closing delimiter must appear on its own line — i.e., preceded by a
/// newline and followed by a newline or end-of-string.  This avoids matching
/// `---` that appears as inline markdown inside the frontmatter region.
fn find_frontmatter_close(after_open: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(rel) = after_open[search_from..].find("\n---") {
        let fm_end = search_from + rel; // index of the `\n` before `---`
        let after_dashes = fm_end + 4; // index just past `\n---`
        let next = after_open.as_bytes().get(after_dashes);
        match next {
            None => return Some((fm_end, after_dashes)),
            Some(b'\r') | Some(b'\n') => {
                // Skip past the newline so body doesn't start with it.
                let body_start = if next == Some(&b'\r')
                    && after_open.as_bytes().get(after_dashes + 1) == Some(&b'\n')
                {
                    after_dashes + 2
                } else {
                    after_dashes + 1
                };
                return Some((fm_end, body_start));
            }
            // Not a standalone `---` line (e.g. `----` or `---foo`).  Keep
            // searching for a later match.
            Some(_) => {
                search_from = after_dashes;
            }
        }
    }
    None
}

/// Validate skill file content before use as a system prompt fragment.
///
/// Rejects content that exceeds the size limit.  This prevents oversized
/// skill files (from shared or writable directories) from bloating the
/// system prompt or serving as a prompt injection vector.
fn validate_skill_content(content: &str, path: &Path) -> Result<()> {
    if content.len() > MAX_SKILL_CONTENT_SIZE {
        return Err(DysonError::Config(format!(
            "skill file {} is too large ({} bytes, max {} bytes)",
            path.display(),
            content.len(),
            MAX_SKILL_CONTENT_SIZE,
        )));
    }
    Ok(())
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
                writeln!(&mut lines, "- {name}: {desc}").unwrap();
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
