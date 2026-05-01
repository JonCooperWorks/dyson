use super::*;
use crate::workspace::InMemoryWorkspace;

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
    assert!(!is_valid_skill_name("my skill")); // space
    assert!(!is_valid_skill_name("-leading")); // leading hyphen
    assert!(!is_valid_skill_name("trailing-")); // trailing hyphen
    assert!(!is_valid_skill_name("my_skill")); // underscore
    assert!(!is_valid_skill_name("../escape")); // path traversal
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
    let dir = std::env::temp_dir().join("test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), &content).unwrap();
    let skill = crate::skill::local::LocalSkill::from_dir(&dir).unwrap();
    // Name comes from the directory name ("test"), not from frontmatter.
    assert_eq!(skill.name(), "test");
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn append_improvements_repairs_malformed_frontmatter() {
    // Missing closing --- — should infer boundary and produce valid output.
    let existing = "---\nname: deploy\ndescription: Old desc\n\nOriginal instructions.\n";
    let improved = append_improvements(existing, "New desc", "New step.");

    assert!(improved.starts_with("---\n"));
    assert!(improved.contains("name: deploy"));
    assert!(improved.contains("description: New desc"));
    assert!(improved.contains("\n---\n")); // proper closing delimiter
    assert!(improved.contains("Original instructions."));
    assert!(improved.contains("## Improvements"));
    assert!(improved.contains("New step."));

    // Verify it parses as a valid skill body.
    let body = crate::skill::local::LocalSkill::parse_body(&improved);
    assert!(
        body.is_some(),
        "repaired output should parse as valid skill"
    );
}

#[tokio::test]
async fn create_skill() {
    let ws = InMemoryWorkspace::new();
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ws = InMemoryWorkspace::new().with_file(
        "skills/existing/SKILL.md",
        "---\nname: existing\ndescription: x\n---\n\nBody.",
    );
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ws = InMemoryWorkspace::new().with_file(
        "skills/deploy/SKILL.md",
        "---\nname: deploy\ndescription: old\n---\n\nOld.",
    );
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ws = InMemoryWorkspace::new().with_file(
        "skills/review/SKILL.md",
        "---\nname: review\ndescription: Reviews code\n---\n\nOriginal approach.",
    );
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    let result = tool
        .run(
            &json!({
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
    let ctx = ToolContext::for_test_with_workspace(ws);
    let tool = SkillCreateTool;

    tool.run(
        &json!({
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
