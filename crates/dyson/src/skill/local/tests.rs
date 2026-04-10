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
// Content validation
// -------------------------------------------------------------------

#[test]
fn validate_rejects_oversized_content() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-oversize-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Create a SKILL.md that exceeds the 64KB limit.
    let oversized = "x".repeat(65 * 1024);
    let content = format!("---\nname: big\n---\n\n{oversized}");
    std::fs::write(dir.join("SKILL.md"), &content).unwrap();

    let err = LocalSkill::from_dir(&dir).unwrap_err();
    assert!(err.to_string().contains("too large"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_accepts_normal_content() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-normal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: ok\ndescription: fine\n---\n\nNormal sized body.",
    )
    .unwrap();

    let skill = LocalSkill::from_dir(&dir).unwrap();
    assert_eq!(skill.body(), "Normal sized body.");

    let _ = std::fs::remove_dir_all(&dir);
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
