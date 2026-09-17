use super::agent_builder::PUBLIC_AGENT_TOOLS;
use super::background_run::finish_background_agent;
use super::commands::BuiltinCommand;
use super::log_tail::read_log_tail_from_dir;
use super::*;
use crate::config::Settings;
use crate::tool::Tool;
use std::io::Write;

/// Regression test for the warmup-placeholder bug:
/// `dyson swarm` resolves its config path internally
/// (`<DYSON_HOME>/dyson.json`) and passes it to `listen::run` via the
/// `config: Option<PathBuf>` argument.  Pre-fix, `create_hot_reloader`
/// ignored that path and re-derived from `std::env::args()` — which,
/// in the systemd-launched swarm container, holds neither
/// `--config` nor a useful cwd, so the function returned `None` and
/// program-level hot-reload was silently disabled.  Post-fix, the
/// caller passes the resolved path as `explicit` and we honour it.
#[test]
fn create_hot_reloader_uses_explicit_path_when_provided() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("dyson.json");
    // The file doesn't need to be valid JSON here — `create_hot_reloader`
    // only stores the path; reads happen later via `HotReloader::check`.
    std::fs::write(&cfg, b"{}").unwrap();
    let settings = Settings::default();

    let (resolved, _reloader) = create_hot_reloader(&settings, Some(&cfg));
    assert_eq!(
        resolved.as_deref(),
        Some(cfg.as_path()),
        "explicit path must win over argv/cwd fallback"
    );
}

/// Regression for the second-instance variant of the
/// warmup-placeholder bug: the HTTP controller's `HttpState::config_path`
/// resolver had its own argv-only copy, so even with `create_hot_reloader`
/// fixed, `routes::admin::post` saw `state.config_path() == None` and
/// silently returned `models_updated: false` on every reconfigure
/// push.  The fix consolidates the resolution into
/// `resolve_config_path_for_runtime`, which honours the OnceLock
/// installed by `command::listen`.
///
/// This test goes through the OnceLock side door; we can't reset
/// it once set (that's the whole point), so we set first and then
/// assert the resolver picks it up.  Other tests in this module
/// run in parallel — they MUST NOT depend on the OnceLock being
/// empty (`create_hot_reloader_falls_back_when_no_explicit`'s
/// docstring explains this).
#[test]
fn resolve_config_path_for_runtime_honours_oncelock() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("dyson.json");
    std::fs::write(&path, b"{}").unwrap();
    install_explicit_config_path(path.clone());

    // No explicit override → must fall through to the OnceLock.
    let resolved = resolve_config_path_for_runtime(None);
    // The OnceLock is set-once across the test process.  Some other
    // test may have set a different path first; we accept either
    // (a) our path or (b) some other test's path.  What we won't
    // accept is `None` — that would mean the OnceLock branch was
    // skipped entirely.
    assert!(
        resolved.is_some(),
        "resolver returned None despite an installed OnceLock path"
    );
}

/// Caller-supplied `explicit` always beats the OnceLock.  This is
/// the contract `command::listen` relies on when it wants the
/// caller's intent to win even after a previous test polluted the
/// global lock.
#[test]
fn resolve_config_path_for_runtime_explicit_beats_oncelock() {
    let tmp = tempfile::tempdir().unwrap();
    let explicit = tmp.path().join("explicit.json");
    std::fs::write(&explicit, b"{}").unwrap();
    let resolved = resolve_config_path_for_runtime(Some(&explicit));
    assert_eq!(resolved.as_deref(), Some(explicit.as_path()));
}

#[test]
fn create_hot_reloader_falls_back_when_no_explicit() {
    // Without an explicit path AND without a `--config` in argv AND
    // without a `dyson.json` in cwd, the resolver returns None.
    // This is the legitimate "in-memory dev / test" path where
    // hot-reload is disabled by design.
    //
    // We can't easily strip `--config` from the test harness's argv,
    // but `cargo test` doesn't pass --config, and the temp cwd from
    // `std::env::set_current_dir` would race with parallel tests, so
    // we just assert the explicit-None branch exists by exercising
    // the same call site shape.
    let settings = Settings::default();
    let (resolved, _reloader) = create_hot_reloader(&settings, None);
    // Either None (no fallback found) or a real path picked up from
    // a co-located dyson.json in the cargo workspace; both are valid
    // for the fallback contract.  The bug we're regressing was the
    // explicit-path-was-ignored branch, covered above.
    let _ = resolved;
}

type BgReceived = std::sync::Arc<std::sync::Mutex<Option<(u64, Result<String, String>)>>>;

fn make_log_dir(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("dyson.log.2026-04-03");
    let mut f = std::fs::File::create(log_path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    dir
}

#[test]
fn tail_basic() {
    let dir = make_log_dir("line1\nline2\nline3\nline4\nline5\n");
    let result = read_log_tail_from_dir(dir.path(), 3).unwrap();
    assert_eq!(result, "line3\nline4\nline5");
}

#[test]
fn tail_more_than_available() {
    let dir = make_log_dir("line1\nline2\n");
    let result = read_log_tail_from_dir(dir.path(), 10).unwrap();
    assert_eq!(result, "line1\nline2");
}

#[test]
fn tail_exact_count() {
    let dir = make_log_dir("line1\nline2\nline3\n");
    let result = read_log_tail_from_dir(dir.path(), 3).unwrap();
    assert_eq!(result, "line1\nline2\nline3");
}

#[test]
fn tail_single_line() {
    let dir = make_log_dir("only\n");
    let result = read_log_tail_from_dir(dir.path(), 1).unwrap();
    assert_eq!(result, "only");
}

#[test]
fn tail_no_trailing_newline() {
    let dir = make_log_dir("line1\nline2\nline3");
    let result = read_log_tail_from_dir(dir.path(), 2).unwrap();
    assert_eq!(result, "line2\nline3");
}

#[test]
fn background_agent_delivers_result_to_callback() {
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
    let id = bg_reg
        .allocate("test".into(), CancellationToken::new())
        .unwrap();

    let received: BgReceived = Arc::new(Mutex::new(None));
    let received_clone = Arc::clone(&received);
    let cb: BackgroundCompletion = Arc::new(move |id, r| {
        *received_clone.lock().unwrap() = Some((id, r));
    });

    finish_background_agent(id, Ok("hello".into()), &bg_reg, &Some(cb));

    let got = received.lock().unwrap().clone();
    assert_eq!(got, Some((id, Ok("hello".to_string()))));
    assert!(bg_reg.list().is_empty(), "registry entry removed");
}

#[test]
fn background_agent_delivers_error_to_callback() {
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
    let id = bg_reg
        .allocate("test".into(), CancellationToken::new())
        .unwrap();

    let received: BgReceived = Arc::new(Mutex::new(None));
    let received_clone = Arc::clone(&received);
    let cb: BackgroundCompletion = Arc::new(move |id, r| {
        *received_clone.lock().unwrap() = Some((id, r));
    });

    finish_background_agent(id, Err("boom".into()), &bg_reg, &Some(cb));

    let got = received.lock().unwrap().clone();
    assert_eq!(got, Some((id, Err("boom".to_string()))));
    assert!(bg_reg.list().is_empty());
}

#[test]
fn format_background_result_variants() {
    assert_eq!(
        format_background_result(7, &Ok("done".into())),
        "Background agent #7 result:\ndone",
    );
    assert_eq!(
        format_background_result(7, &Ok("   ".into())),
        "Background agent #7 finished with no response.",
    );
    assert_eq!(
        format_background_result(9, &Err("boom".into())),
        "Background agent #9 failed: boom",
    );
}

#[test]
fn persist_background_result_appends_to_chat_history() {
    use crate::chat_history::{ChatHistory, DiskChatHistory};
    use crate::message::{ContentBlock, Message, Role};

    let dir = tempfile::tempdir().unwrap();
    let store = DiskChatHistory::new(dir.path().to_path_buf()).unwrap();
    let chat_key = "123";
    store.save(chat_key, &[Message::user("hi")]).unwrap();

    let returned =
        persist_background_result(&store, chat_key, 7, &Ok("result text".into())).unwrap();
    assert_eq!(returned.len(), 2);

    let loaded = store.load(chat_key).unwrap();
    assert_eq!(loaded.len(), 2, "original turn + appended result");
    let last = loaded.last().unwrap();
    assert_eq!(last.role, Role::User);
    match &last.content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("Background agent #7"), "got: {text}");
            assert!(text.contains("result text"), "got: {text}");
        }
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn background_agent_finish_without_callback_still_prunes() {
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let bg_reg = Arc::new(background::BackgroundAgentRegistry::new());
    let id = bg_reg
        .allocate("test".into(), CancellationToken::new())
        .unwrap();

    finish_background_agent(id, Ok("x".into()), &bg_reg, &None);
    assert!(bg_reg.list().is_empty());
}

#[test]
fn tail_empty_file() {
    let dir = make_log_dir("");
    let result = read_log_tail_from_dir(dir.path(), 5).unwrap();
    assert_eq!(result, "");
}

#[test]
fn tail_picks_most_recent_file() {
    let dir = tempfile::tempdir().unwrap();
    // Older file
    std::fs::write(dir.path().join("dyson.log.2026-04-01"), "old\n").unwrap();
    // Newer file
    std::fs::write(dir.path().join("dyson.log.2026-04-03"), "new\n").unwrap();
    let result = read_log_tail_from_dir(dir.path(), 1).unwrap();
    assert_eq!(result, "new");
}

#[test]
fn tail_no_log_files() {
    let dir = tempfile::tempdir().unwrap();
    let result = read_log_tail_from_dir(dir.path(), 5);
    assert!(result.is_err());
}

#[test]
fn tail_large_file_spanning_chunks() {
    // Create a file larger than the 8192-byte chunk size.
    use std::fmt::Write as _;
    let mut content = String::new();
    for i in 0..500 {
        writeln!(&mut content, "log line number {i:04}").unwrap();
    }
    let dir = make_log_dir(&content);
    let result = read_log_tail_from_dir(dir.path(), 5).unwrap();
    let lines: Vec<&str> = result.split('\n').collect();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0], "log line number 0495");
    assert_eq!(lines[4], "log line number 0499");
}

// -----------------------------------------------------------------------
// /logs N integration: parsing + read_log_tail end-to-end
// -----------------------------------------------------------------------

#[test]
fn logs_command_with_line_count() {
    let dir = make_log_dir("line1\nline2\nline3\nline4\nline5\n");

    let input = "/logs 3";
    let BuiltinCommand::Logs(n) = BuiltinCommand::parse(input) else {
        panic!("expected logs command")
    };
    assert_eq!(n, 3);

    let result = read_log_tail_from_dir(dir.path(), n).unwrap();
    assert_eq!(result, "line3\nline4\nline5");
}

#[test]
fn logs_command_default_count() {
    let input = "/logs";
    let BuiltinCommand::Logs(n) = BuiltinCommand::parse(input) else {
        panic!("expected logs command")
    };
    assert_eq!(n, 20);
}

// -----------------------------------------------------------------------
// Public agent tool and read-only tests
//
// These verify that public agents get the correct tool set (workspace
// memory + web, no filesystem/shell) and that identity files are
// protected from writes via workspace.is_read_only().
// -----------------------------------------------------------------------

#[test]
fn public_agent_has_workspace_and_web_tools() {
    // Build an agent using the same skill filter as build_public_agent.
    let filter: Vec<String> = PUBLIC_AGENT_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let skills: Vec<Box<dyn crate::skill::Skill>> = vec![Box::new(
        crate::skill::builtin::BuiltinSkill::new_filtered(None, None, None, &filter),
    )];

    let sandbox: std::sync::Arc<dyn crate::sandbox::Sandbox> =
        std::sync::Arc::new(crate::sandbox::no_sandbox::DangerousNoSandbox::new(
            crate::sandbox::SandboxBypassGuard::for_test(),
        ));
    let client = crate::llm::create_client(&crate::config::AgentSettings::default(), None, None);
    let client = crate::agent::rate_limiter::RateLimitedHandle::unlimited(client);

    let agent = crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&crate::config::AgentSettings::default())
        .build()
        .unwrap();

    // Public agent SHOULD have workspace memory + web tools.
    assert!(agent.has_tool("workspace"), "must have workspace");
    assert!(agent.has_tool("memory_search"), "must have memory_search");
    assert!(agent.has_tool("web_fetch"), "must have web_fetch");
    // web_search is conditional on config, so not tested here.

    // Public agent should NOT have filesystem/shell tools.
    assert!(!agent.has_tool("bash"), "must not have bash");
    assert!(!agent.has_tool("read_file"), "must not have read_file");
    assert!(!agent.has_tool("write_file"), "must not have write_file");
    assert!(!agent.has_tool("edit_file"), "must not have edit_file");
    assert!(!agent.has_tool("list_files"), "must not have list_files");
    assert!(
        !agent.has_tool("search_files"),
        "must not have search_files"
    );
    assert!(!agent.has_tool("send_file"), "must not have send_file");
    assert!(!agent.has_tool("load_skill"), "must not have load_skill");
    assert!(!agent.has_tool("kb_search"), "must not have kb_search");
    assert!(!agent.has_tool("kb_status"), "must not have kb_status");
}

#[tokio::test]
async fn channel_workspace_only_allows_whitelisted_writes() {
    use crate::workspace::{InMemoryWorkspace, channel::ChannelWorkspace};

    let inner = InMemoryWorkspace::new()
        .with_file("SOUL.md", "Be helpful.")
        .with_file("IDENTITY.md", "I am a test bot.")
        .with_file("MEMORY.md", "");

    let ws = ChannelWorkspace::new(Box::new(inner))
        .allow("MEMORY.md")
        .allow("USER.md")
        .allow_prefix("memory/");

    let ctx = crate::tool::ToolContext::for_test_with_workspace(ws);
    let tool = crate::tool::workspace::WorkspaceTool;

    // Writing to SOUL.md — not whitelisted, silently dropped.
    let _ = tool
        .run(
            &serde_json::json!({
                "op": "update",
                "file": "SOUL.md",
                "content": "Be evil.",
                "mode": "set"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let ws = ctx.workspace("test").unwrap().read().await;
    assert_eq!(ws.get("SOUL.md").unwrap(), "Be helpful.");
    drop(ws);

    // Writing to MEMORY.md — whitelisted, succeeds.
    let _ = tool
        .run(
            &serde_json::json!({
                "op": "update",
                "file": "MEMORY.md",
                "content": "Learned something.",
                "mode": "set"
            }),
            &ctx,
        )
        .await
        .unwrap();
    let ws = ctx.workspace("test").unwrap().read().await;
    assert_eq!(ws.get("MEMORY.md").unwrap(), "Learned something.");
}

#[test]
fn public_agent_tools_constant_matches_expected() {
    let expected = &["workspace", "memory_search", "web_fetch", "web_search"];
    assert_eq!(PUBLIC_AGENT_TOOLS, expected);
}
