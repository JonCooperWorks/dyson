use super::*;
use crate::skill::Skill;
use std::path::PathBuf;

/// A context whose sandbox is `--dangerous-no-sandbox`: scripts run
/// unwrapped, as they do on managed dysons.
fn unsandboxed_ctx() -> crate::tool::ToolContext {
    crate::tool::ToolContext {
        sandbox: Some(std::sync::Arc::new(
            crate::sandbox::no_sandbox::DangerousNoSandbox::new(
                crate::sandbox::SandboxBypassGuard::for_test(),
            ),
        )),
        ..crate::tool::ToolContext::from_cwd().unwrap()
    }
}

/// Write a script skill into `dir` and load it.
fn script_skill(dir: &std::path::Path, script: &str) -> LocalSkill {
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Script\n\nInstructions only.\n").unwrap();
    std::fs::write(dir.join("bin/run.sh"), script).unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        format!(
            r#"{{
              "schema_version":2,
              "name":"{name}",
              "description":"Script",
              "slash_command":"/skill-script",
              "execution":{{"kind":"script","entrypoint":"bin/run.sh","argument_mode":"raw","timeout_ms":5000}}
            }}"#
        ),
    )
    .unwrap();
    LocalSkill::from_dir(dir).unwrap()
}

/// Records what a script skill asks for and answers with a fixed decision.
struct ScriptRecordingSandbox {
    seen: std::sync::Mutex<Vec<(String, PathBuf)>>,
    allow_with: Option<String>,
}

#[async_trait::async_trait]
impl crate::sandbox::Sandbox for ScriptRecordingSandbox {
    async fn check(
        &self,
        _tool_name: &str,
        input: &serde_json::Value,
        _ctx: &crate::tool::ToolContext,
    ) -> dyson_core::error::Result<crate::sandbox::SandboxDecision> {
        Ok(crate::sandbox::SandboxDecision::Allow {
            input: input.clone(),
        })
    }

    async fn check_skill_script(
        &self,
        command: &str,
        skill_dir: &std::path::Path,
        _ctx: &crate::tool::ToolContext,
    ) -> dyson_core::error::Result<crate::sandbox::SandboxDecision> {
        self.seen
            .lock()
            .unwrap()
            .push((command.to_string(), skill_dir.to_path_buf()));
        Ok(match &self.allow_with {
            Some(command) => crate::sandbox::SandboxDecision::Allow {
                input: serde_json::json!({ "command": command }),
            },
            None => crate::sandbox::SandboxDecision::Deny {
                reason: "recording sandbox denies".into(),
            },
        })
    }
}

fn test_path() -> PathBuf {
    PathBuf::from("/fake/skills/test-skill/SKILL.md")
}

// -------------------------------------------------------------------
// Frontmatter parsing
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
fn parse_frontmatter_empty_body_falls_back_to_whole_file() {
    // Freeform skills never fail for non-empty content.  A frontmatter-
    // only file is loaded with the whole content as the body so the skill
    // remains usable — the author may rely on the description alone, or
    // add a body later without re-running validation.
    let content = "\
---
name: empty-body
description: has just metadata
---
";
    let skill = LocalSkill::parse(content, "empty-body", &test_path()).unwrap();
    assert_eq!(skill.name, "empty-body");
    assert_eq!(skill.description, "has just metadata");
    assert!(skill.body.contains("name: empty-body"));
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
fn parse_malformed_frontmatter_only_falls_back_to_whole_file() {
    // All frontmatter, no closing `---`, no body — previously rejected,
    // now loads with the raw file as the body so the author doesn't lose
    // their skill to a syntax nit.
    let content = "---\nname: pastebin\ndescription: Posts things. No auth, no API key.";
    let skill = LocalSkill::parse(content, "pastebin", &test_path()).unwrap();
    assert_eq!(skill.name, "pastebin");
    assert_eq!(skill.description, "Posts things. No auth, no API key.");
    assert!(skill.body.contains("name: pastebin"));
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
    assert_eq!(
        skill.description,
        "Reviews code for quality and security issues"
    );
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
// Freeform / robustness tests — skills are text, not a strict schema.
// -------------------------------------------------------------------

#[test]
fn parse_markdown_starting_with_horizontal_rule_is_not_frontmatter() {
    // `---` on its own line followed by markdown is a horizontal rule in
    // many markdown dialects.  We should NOT misinterpret it as an empty
    // frontmatter block and strip it.
    let content = "\
---

# My Skill

Do the thing.
";
    let skill = LocalSkill::parse(content, "my-skill", &test_path()).unwrap();
    assert_eq!(skill.name, "my-skill");
    // The full content (including the leading rule) should survive as body.
    assert!(skill.body.contains("# My Skill"));
    assert!(skill.body.contains("Do the thing."));
}

#[test]
fn parse_line_starting_with_dashes_not_frontmatter() {
    // `---foo` is not a frontmatter opener (no newline after the dashes).
    let content = "---foo bar\n\nRest of the skill.\n";
    let skill = LocalSkill::parse(content, "dashes", &test_path()).unwrap();
    assert!(skill.body.contains("---foo bar") || skill.body.contains("Rest of the skill"));
}

#[test]
fn parse_four_dashes_not_frontmatter_close() {
    // A line of `----` inside frontmatter is not a close — it has an
    // extra char.  We keep searching for a real `\n---\n` close.
    let content = "\
---
name: dashes
description: four dashes in body
----
body starts here
---

Real body text.
";
    let skill = LocalSkill::parse(content, "dashes", &test_path()).unwrap();
    assert_eq!(skill.description, "four dashes in body");
    assert!(skill.body.contains("Real body text."));
}

#[test]
fn parse_body_with_embedded_horizontal_rules_preserved() {
    // A body that uses `---` as a section separator must not be truncated.
    let content = "\
---
name: multi
description: has sections
---

Section A.

---

Section B.
";
    let skill = LocalSkill::parse(content, "multi", &test_path()).unwrap();
    assert!(skill.body.contains("Section A."));
    assert!(skill.body.contains("Section B."));
    assert!(skill.body.contains("---"));
}

#[test]
fn parse_single_line_skill_loads() {
    // Absolute minimum: one line of text, no structure at all.
    let skill = LocalSkill::parse("Just do the thing.", "quick", &test_path()).unwrap();
    assert_eq!(skill.name, "quick");
    assert_eq!(skill.body, "Just do the thing.");
}

#[test]
fn parse_never_errors_on_non_empty_content() {
    // Property-style check: a bunch of weird-but-non-empty inputs should
    // all parse successfully.
    let cases = [
        "---",
        "---\n",
        "---\n---",
        "just text",
        "---\nnot: real: frontmatter",
        "#!/bin/bash\necho hi",
        "{\"json\": \"object\"}",
        "<!-- comment -->\nskill body",
    ];
    for content in cases {
        let skill = LocalSkill::parse(content, "weird", &test_path())
            .unwrap_or_else(|e| panic!("failed to parse {content:?}: {e}"));
        assert!(!skill.body.is_empty(), "body empty for {content:?}");
    }
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

#[test]
fn manifest_v1_instruction_only_has_no_tools() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-v1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "V1 skill\n\nInstructions.\n").unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        r#"{"schema_version":1,"name":"dyson-skill-v1-PLACEHOLDER","description":"from manifest"}"#,
    )
    .unwrap();
    let manifest = dir.join("dyson-skill.json");
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        manifest,
        format!(r#"{{"schema_version":1,"name":"{name}","description":"from manifest"}}"#),
    )
    .unwrap();

    let skill = LocalSkill::from_dir(&dir).unwrap();
    assert_eq!(skill.skill_description(), "from manifest");
    assert!(skill.tools().is_empty());
    assert_eq!(skill.slash_command(), None);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn manifest_v2_script_registers_and_executes_tool() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-echo-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Echoes input\n\nInstructions only.\n").unwrap();
    std::fs::write(dir.join("bin/run.sh"), "cat\n").unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        format!(
            r#"{{
              "schema_version":2,
              "name":"{name}",
              "description":"Echoes input",
              "slash_command":"/skill-echo",
              "execution":{{"kind":"script","entrypoint":"bin/run.sh","argument_mode":"raw","timeout_ms":5000}}
            }}"#
        ),
    )
    .unwrap();

    let skill = LocalSkill::from_dir(&dir).unwrap();
    assert_eq!(skill.slash_command(), Some("/skill-echo"));
    assert_eq!(skill.tools().len(), 1);
    assert_eq!(skill.tools()[0].name(), manifest::tool_name_for_skill(name));

    let out = skill.tools()[0]
        .run(&serde_json::json!({"raw": "hello"}), &unsandboxed_ctx())
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("\"raw\":\"hello\""));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn script_that_ignores_stdin_still_returns_stdout() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-no-stdin-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Echoes input\n\nInstructions only.\n").unwrap();
    std::fs::write(
        dir.join("bin/run.sh"),
        "#!/usr/bin/env bash\nset -euo pipefail\nprintf 'ignored stdin ok\\n'\n",
    )
    .unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        format!(
            r#"{{
              "schema_version":2,
              "name":"{name}",
              "description":"Echoes input",
              "slash_command":"/skill-echo",
              "execution":{{"kind":"script","entrypoint":"bin/run.sh","argument_mode":"raw","timeout_ms":5000}}
            }}"#
        ),
    )
    .unwrap();

    let skill = LocalSkill::from_dir(&dir).unwrap();
    let out = skill.tools()[0]
        .run(&serde_json::json!({"raw": "hello"}), &unsandboxed_ctx())
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "ignored stdin ok");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_manifest_entrypoint_rejected() {
    let dir = std::env::temp_dir().join(format!("dyson-skill-bad-entry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Bad\n\nBody.\n").unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        format!(
            r#"{{
              "schema_version":2,
              "name":"{name}",
              "slash_command":"/bad-entry",
              "execution":{{"kind":"script","entrypoint":"../run.sh"}}
            }}"#
        ),
    )
    .unwrap();

    let err = LocalSkill::from_dir(&dir).unwrap_err();
    assert!(err.to_string().contains("traversal"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_skill_body_remains_instructions_only() {
    let content = "---\nname: hybrid\ndescription: d\n---\n\nOnly instructions.\n";
    assert_eq!(
        LocalSkill::parse_body(content),
        Some("Only instructions.".to_string())
    );
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
    let skill = SkillListSkill::new(&[]);
    assert!(skill.system_prompt().is_none());
}

#[test]
fn skill_list_builds_prompt() {
    let skills = vec![
        SkillListEntry {
            name: "code-review".into(),
            description: "Reviews code".into(),
            slash_command: Some("/review".into()),
            executable: true,
        },
        SkillListEntry {
            name: "deploy".into(),
            description: "Deploys things".into(),
            slash_command: None,
            executable: false,
        },
    ];
    let skill = SkillListSkill::new(&skills);
    let prompt = skill.system_prompt().unwrap();
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("- code-review: Reviews code (slash command: /review)"));
    assert!(prompt.contains("- deploy: Deploys things"));
    assert!(prompt.contains("</available_skills>"));
    assert!(prompt.contains("load_skill"));
}

#[tokio::test]
async fn script_writing_more_than_a_pipe_buffer_is_not_a_timeout() {
    // 1 MiB on stdout and 256 KiB on stderr, far past a 64 KiB pipe
    // buffer.  Before the pipes were drained concurrently the script
    // blocked on its write until the timeout and was reported as one.
    let dir = std::env::temp_dir().join(format!("dyson-skill-big-out-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "Big output\n\nInstructions only.\n").unwrap();
    std::fs::write(
        dir.join("bin/run.sh"),
        "head -c 262144 /dev/zero | tr '\\0' e >&2\nhead -c 1048576 /dev/zero | tr '\\0' o\n",
    )
    .unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap();
    std::fs::write(
        dir.join("dyson-skill.json"),
        format!(
            r#"{{
              "schema_version":2,
              "name":"{name}",
              "description":"Big output",
              "slash_command":"/skill-big",
              "execution":{{"kind":"script","entrypoint":"bin/run.sh","argument_mode":"raw","timeout_ms":5000}}
            }}"#
        ),
    )
    .unwrap();

    let skill = LocalSkill::from_dir(&dir).unwrap();
    let started = std::time::Instant::now();
    let out = skill.tools()[0]
        .run(&serde_json::json!({"raw": ""}), &unsandboxed_ctx())
        .await
        .unwrap();
    assert!(
        !out.is_error,
        "{}",
        &out.content[..out.content.len().min(200)]
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    let (stdout, stderr) = out.content.split_once("\n\n[stderr]\n").unwrap();
    assert_eq!(stdout.len(), 1_048_576);
    assert_eq!(stderr.len(), 262_144);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn script_skill_runs_the_command_its_sandbox_returns() {
    // The script must be started by whatever the sandbox hands back (the
    // bwrap / container wrapper under PolicySandbox), not by a bare
    // `bash <entrypoint>`.  The recording sandbox substitutes a command, so
    // the output proves which one ran.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skill-wrapped");
    let skill = script_skill(&dir, "echo unwrapped\n");
    let sandbox = std::sync::Arc::new(ScriptRecordingSandbox {
        seen: std::sync::Mutex::new(Vec::new()),
        allow_with: Some("echo wrapped".into()),
    });
    let ctx = crate::tool::ToolContext {
        sandbox: Some(sandbox.clone()),
        ..crate::tool::ToolContext::from_cwd().unwrap()
    };

    let out = skill.tools()[0]
        .run(&serde_json::json!({"raw": ""}), &ctx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "wrapped");

    let seen = sandbox.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let canonical = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(seen[0].1, canonical);
    assert_eq!(
        seen[0].0,
        format!(
            "cd '{}' && exec bash '{}'",
            canonical.display(),
            canonical.join("bin/run.sh").display()
        )
    );
}

#[tokio::test]
async fn script_skill_denied_by_sandbox_never_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skill-denied");
    let marker = tmp.path().join("ran");
    let skill = script_skill(&dir, &format!("touch '{}'\n", marker.display()));
    let ctx = crate::tool::ToolContext {
        sandbox: Some(std::sync::Arc::new(ScriptRecordingSandbox {
            seen: std::sync::Mutex::new(Vec::new()),
            allow_with: None,
        })),
        ..crate::tool::ToolContext::from_cwd().unwrap()
    };

    let out = skill.tools()[0]
        .run(&serde_json::json!({"raw": ""}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("denied by sandbox"), "{}", out.content);
    assert!(!marker.exists(), "a denied script must not run");
}

#[tokio::test]
async fn script_skill_without_a_sandbox_refuses_to_run() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skill-no-sandbox");
    let marker = tmp.path().join("ran");
    let skill = script_skill(&dir, &format!("touch '{}'\n", marker.display()));

    let out = skill.tools()[0]
        .run(
            &serde_json::json!({"raw": ""}),
            &crate::tool::ToolContext::from_cwd().unwrap(),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("no sandbox"), "{}", out.content);
    assert!(!marker.exists());
}
