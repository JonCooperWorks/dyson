//! Validate cross-references after resolving configuration.
use crate::config::{Settings, SkillConfig};
use crate::error::{DysonError, Result};

/// Refuse to boot with an unconfigured agent model.
///
/// Dyson no longer ships hardcoded per-provider defaults, so every config
/// must resolve to an explicit model id — either `agent.model` or the first
/// entry of the active provider's `models` array.  A missing model used to
/// silently fall back to Sonnet, which billed users for a model they never
/// asked for.
pub(super) fn validate_agent_model(settings: &Settings) -> Result<()> {
    if settings.agent.model.trim().is_empty() {
        return Err(DysonError::Config(
            "no model configured for the active agent.  Set `agent.model` or \
             populate the active provider's `models` array in dyson.json.  \
             Dyson no longer falls back to a hardcoded default so the user \
             is always in control of what they pay for."
                .into(),
        ));
    }
    Ok(())
}

/// Reject subagent configs whose `provider` is not `"default"` or a
/// known entry in `settings.providers`.  Tool-filter names are checked
/// later (in `SubagentSkill::new`) because tool names aren't known
/// until skills have loaded; that path degrades to a `warn!`.
pub fn validate_subagent_configs(settings: &Settings) -> Result<()> {
    for skill in &settings.skills {
        let SkillConfig::Subagent(cfg) = skill else {
            continue;
        };
        for agent in &cfg.agents {
            if agent.provider != "default" && !settings.providers.contains_key(&agent.provider) {
                let known: Vec<&str> = settings.providers.keys().map(String::as_str).collect();
                return Err(DysonError::Config(format!(
                    "subagent '{}' references unknown provider '{}'. \
                     Known providers: [{}] or \"default\"",
                    agent.name,
                    agent.provider,
                    known.join(", "),
                )));
            }
        }
    }
    Ok(())
}
