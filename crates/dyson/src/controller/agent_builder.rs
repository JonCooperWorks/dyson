//! Construct private and restricted public agents.
use super::{AgentMode, ClientRegistry};
use crate::config::Settings;

/// Build an agent from settings.
///
/// `mode` controls the trust level — `AgentMode::Private` builds a
/// full-featured agent, `AgentMode::Public` builds a hardened agent with
/// per-channel workspace memory and web tools.  See `docs/public-agents.md`.
///
/// `channel_id` is required for `AgentMode::Public` — it determines which
/// per-channel workspace subdirectory to use.
///
/// The `client` handle comes from a [`ClientRegistry`] — all agents
/// share the same LLM client and rate-limit window per provider.
///
/// Every controller should use this instead of building agents manually.
/// The mode is the single point of control — individual controllers just
/// declare the trust level, and this function handles the rest.
pub async fn build_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    mode: AgentMode,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    registry: &ClientRegistry,
    channel_id: Option<&str>,
) -> crate::Result<crate::agent::Agent> {
    if mode == AgentMode::Public {
        let ch = channel_id.unwrap_or("unknown");
        return build_public_agent(settings, controller_prompt, client, ch);
    }

    // --- Private agent: full tools, workspace, dreams ---

    let workspace = crate::workspace::create_workspace(&settings.workspace)?;

    let mut agent_settings = settings.agent.clone();

    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    if let Some(prompt) = controller_prompt {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(prompt);
    }

    let workspace: crate::workspace::WorkspaceHandle =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    let nudge_interval = {
        let ws = workspace.read().await;
        ws.nudge_interval()
    };

    let sandbox =
        crate::sandbox::create_sandbox(&settings.sandbox, settings.sandbox_bypass.clone());
    let skills = {
        let ws = workspace.read().await;
        crate::skill::create_skills(
            settings,
            Some(&**ws),
            std::sync::Arc::clone(&sandbox),
            Some(std::sync::Arc::clone(&workspace)),
            registry,
        )
        .await
    };

    let transcriber = crate::media::audio::create_transcriber(settings.transcriber.as_ref());

    let mut builder = crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .transcriber(transcriber);

    // Create advisor if smartest_model is configured.
    // Format: "provider_name/model" (e.g. "openrouter/glm-5", "claude/claude-opus-4-6").
    // Skip if the advisor resolves to the same model the executor is already using.
    if let Some(ref smartest_model) = settings.agent.smartest_model {
        if let Some((provider_name, advisor_model)) = smartest_model.split_once('/') {
            // Skip if the advisor is the currently loaded model.
            let is_same_model = settings.providers.get(provider_name).is_some_and(|pc| {
                pc.provider_type == settings.agent.provider && advisor_model == settings.agent.model
            });

            if !is_same_model {
                let advisor_provider_type = settings
                    .providers
                    .get(provider_name)
                    .map(|pc| pc.provider_type.clone())
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            provider = provider_name,
                            "advisor provider not found, falling back to generic"
                        );
                        crate::config::LlmProvider::OpenAi // will use generic path
                    });

                let advisor_client = match registry.get(provider_name) {
                    Ok(handle) => handle,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to create advisor client, skipping");
                        return builder.build();
                    }
                };

                let advisor = crate::advisor::create_advisor(
                    &settings.agent.provider,
                    &advisor_provider_type,
                    advisor_model,
                    advisor_client,
                );
                builder = builder.advisor(advisor);
            } else {
                tracing::info!(
                    smartest_model = smartest_model.as_str(),
                    "advisor model is the currently loaded model, skipping"
                );
            }
        } else {
            tracing::warn!(
                smartest_model = smartest_model.as_str(),
                "smartest_model must be in 'provider/model' format (e.g. 'claude/claude-opus-4-6')"
            );
        }
    }

    builder.build()
}

/// Tools available to public agents — workspace memory + web research.
pub(super) const PUBLIC_AGENT_TOOLS: &[&str] =
    &["workspace", "memory_search", "web_fetch", "web_search"];

/// Build a public agent with a per-channel workspace.
///
/// The agent gets its own workspace under `{main_workspace}/channels/{channel_id}/`,
/// with SOUL.md and IDENTITY.md symlinked from the main workspace (read-only).
/// Tools are restricted to workspace memory operations and web research.
/// Sandbox is always enforced.
fn build_public_agent(
    settings: &Settings,
    controller_prompt: Option<&str>,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    channel_id: &str,
) -> crate::Result<crate::agent::Agent> {
    let filter: Vec<String> = PUBLIC_AGENT_TOOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let ig_provider = settings
        .agent
        .image_generation_provider
        .as_ref()
        .and_then(|name| settings.providers.get(name));
    let skills: Vec<Box<dyn crate::skill::Skill>> =
        vec![Box::new(crate::skill::builtin::BuiltinSkill::new_filtered(
            settings.web_search.as_ref(),
            ig_provider,
            settings.agent.image_generation_model.as_deref(),
            &filter,
        ))];

    let workspace = crate::workspace::create_channel_workspace(&settings.workspace, channel_id)?;

    let mut agent_settings = settings.agent.clone();

    let ws_prompt = workspace.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    agent_settings.system_prompt.push_str(
        "\n\nYou are a public-facing agent. You can search the web, fetch web pages, \
         and maintain persistent memory for this channel. You do NOT have access \
         to the filesystem, shell commands, or the operator's private workspace. \
         Be concise and cite your sources.",
    );

    if let Some(prompt) = controller_prompt {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(prompt);
    }

    let nudge_interval = workspace.nudge_interval();

    let workspace: crate::workspace::WorkspaceHandle =
        std::sync::Arc::new(tokio::sync::RwLock::new(workspace));

    // SECURITY: Always None — public agent sandbox is never disabled.
    let sandbox = crate::sandbox::create_sandbox(&settings.sandbox, None);

    crate::agent::Agent::builder(client, sandbox)
        .skills(skills)
        .settings(&agent_settings)
        .workspace(workspace)
        .nudge_interval(nudge_interval)
        .build()
}
