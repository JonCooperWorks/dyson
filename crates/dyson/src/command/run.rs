// ===========================================================================
// dyson run — single-shot: run one prompt and exit.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Runs the agent with a single prompt and exits.  Useful for scripting,
//   one-off questions, or testing without starting the full controller loop.
//
// How it differs from `dyson listen`:
//   - No controller lifecycle — just creates an agent directly
//   - No conversation persistence — one prompt, one response
//   - Uses TerminalOutput for stdout rendering
//   - Exits immediately after the agent finishes
// ===========================================================================

use std::path::PathBuf;

/// Run `dyson run`.
pub async fn run(
    prompt: String,
    config: Option<PathBuf>,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> dyson::error::Result<()> {
    let config_path = super::resolve_config_path(config);

    let mut settings = dyson::config::loader::load_settings(config_path.as_deref())?;
    super::apply_overrides(
        &mut settings,
        dangerous_no_sandbox,
        provider,
        base_url,
        workspace,
    )?;

    let workspace = dyson::workspace::create_workspace(&settings.workspace)?;
    let mut agent_settings = settings.agent.clone();
    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    // Wrap workspace in Arc<RwLock> for shared tool access.
    let workspace: std::sync::Arc<tokio::sync::RwLock<Box<dyn dyson::workspace::Workspace>>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    let sandbox = dyson::sandbox::create_sandbox(&settings.sandbox, settings.dangerous_no_sandbox);
    let registry =
        dyson::controller::ClientRegistry::new(&settings, Some(std::sync::Arc::clone(&workspace)));
    let client = registry.get_default();
    let skills = {
        let ws = workspace.read().await;
        dyson::skill::create_skills(
            &settings,
            Some(&**ws),
            std::sync::Arc::clone(&sandbox),
            Some(std::sync::Arc::clone(&workspace)),
            &registry,
        )
        .await
    };
    let nudge_interval = {
        let ws = workspace.read().await;
        ws.nudge_interval()
    };
    let mut agent = dyson::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .build()?;
    let mut output = dyson::controller::terminal::TerminalOutput::new();
    agent.run(&prompt, &mut output).await?;
    println!();

    Ok(())
}
