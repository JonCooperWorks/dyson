//! One read, patch, and atomic write for runtime configuration.

use super::TelegramProxyConfigure;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct ConfigureConfigPatch<'a> {
    pub(super) http_auth_hash: Option<&'a str>,
    pub(super) provider_name: Option<&'a str>,
    pub(super) models: Option<&'a [String]>,
    pub(super) api_key: Option<&'a str>,
    pub(super) base_url: Option<&'a str>,
    pub(super) active_selection: Option<(&'a str, &'a str)>,
    pub(super) image_provider_block: Option<(&'a str, &'a Value)>,
    pub(super) image_provider: Option<&'a str>,
    pub(super) image_model: Option<&'a str>,
    pub(super) tools: Option<&'a [String]>,
    pub(super) reset_skills: bool,
    pub(super) mcp_servers: Option<&'a serde_json::Map<String, Value>>,
    pub(super) telegram_proxy: Option<&'a TelegramProxyConfigure>,
    pub(super) refresh_subscription_models: bool,
}

#[derive(Default)]
pub(super) struct AppliedConfigPatch {
    pub(super) http_auth_changed: bool,
    pub(super) provider_changed: bool,
    pub(super) model_selection_changed: bool,
    pub(super) image_changed: bool,
    pub(super) skills_changed: bool,
    pub(super) mcp_changed: bool,
    pub(super) telegram_changed: bool,
}

impl AppliedConfigPatch {
    pub(super) fn any(&self) -> bool {
        self.http_auth_changed
            || self.provider_changed
            || self.model_selection_changed
            || self.image_changed
            || self.skills_changed
            || self.mcp_changed
            || self.telegram_changed
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ConfigureConfigPatchError {
    #[error("HTTP controller missing from config")]
    HttpControllerMissing,
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("config root is not an object")]
    RootNotObject,
    #[error("config has no agent.provider - can't tell which provider's config to patch")]
    MissingAgentProvider,
    #[error("config has no providers object")]
    MissingProvidersObject,
    #[error("config has no providers.{0}")]
    MissingProvider(String),
    #[error("config providers is not an object")]
    ProvidersNotObject,
    #[error("config agent is not an object")]
    AgentNotObject,
    #[error("skills is not an object")]
    SkillsNotObject,
    #[error("skills.builtin is not an object")]
    SkillsBuiltinNotObject,
    #[error("serialise: {0}")]
    Serialize(serde_json::Error),
    #[error("write tmp: {0}")]
    WriteTmp(std::io::Error),
    #[error("rename: {0}")]
    Rename(std::io::Error),
}

pub(super) async fn patch_config_once(
    path: &std::path::Path,
    patch: ConfigureConfigPatch<'_>,
) -> std::result::Result<AppliedConfigPatch, ConfigureConfigPatchError> {
    let raw = tokio::fs::read_to_string(path).await.map_err(|source| {
        ConfigureConfigPatchError::Read {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let mut doc: Value =
        serde_json::from_str(&raw).map_err(|source| ConfigureConfigPatchError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let mut applied = AppliedConfigPatch::default();
    if let Some(hash) = patch.http_auth_hash {
        let controllers = doc
            .get_mut("controllers")
            .and_then(Value::as_array_mut)
            .ok_or(ConfigureConfigPatchError::HttpControllerMissing)?;
        let mut found = false;
        for controller in controllers {
            if controller.get("type").and_then(Value::as_str) == Some("http") {
                controller["auth"] = serde_json::json!({"type":"swarm", "hash":hash});
                found = true;
            }
        }
        if !found {
            return Err(ConfigureConfigPatchError::HttpControllerMissing);
        }
        applied.http_auth_changed = true;
    }

    if patch.refresh_subscription_models {
        applied.provider_changed = patch_subscription_models_doc(&mut doc)?;
    }

    if patch.models.is_some() || patch.api_key.is_some() || patch.base_url.is_some() {
        patch_provider_doc(
            &mut doc,
            patch.provider_name,
            patch.models,
            patch.api_key,
            patch.base_url,
        )?;
        applied.provider_changed = true;
    }
    if let Some((provider, model)) = patch.active_selection {
        applied.model_selection_changed = patch_active_selection_doc(&mut doc, provider, model)?;
    }
    if patch.image_provider_block.is_some()
        || patch.image_provider.is_some()
        || patch.image_model.is_some()
    {
        patch_image_generation_doc(
            &mut doc,
            patch.image_provider_block,
            patch.image_provider,
            patch.image_model,
        )?;
        applied.image_changed = true;
    }
    if let Some(tools) = patch.tools {
        applied.skills_changed = set_skills_tools_doc(&mut doc, tools)?;
    } else if patch.reset_skills {
        applied.skills_changed = clear_skills_doc(&mut doc)?;
    }
    if let Some(servers) = patch.mcp_servers {
        applied.mcp_changed = patch_mcp_servers_doc(&mut doc, servers)?;
    }
    if let Some(proxy) = patch.telegram_proxy {
        applied.telegram_changed = patch_telegram_controller_doc(&mut doc, proxy)?;
    }

    if applied.any() {
        write_config_doc(path, &doc).await?;
    }
    Ok(applied)
}

/// Upsert native subscription providers from the shared catalogue. Runtime
/// configure must own this migration because a stateful Cube rotation carries
/// the source instance's old `dyson.json` onto the new image.
pub(super) fn patch_subscription_models_doc(
    doc: &mut Value,
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    let active_provider = doc
        .get("agent")
        .and_then(|agent| agent.get("provider"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let active_model = doc
        .get("agent")
        .and_then(|agent| agent.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let desired = [
        (
            "chatgpt-subscription",
            "codex",
            crate::subscription_models::CHATGPT,
        ),
        (
            "claude-subscription",
            "claude-code",
            crate::subscription_models::CLAUDE,
        ),
    ];
    let replacement_model = desired.iter().find_map(|(name, _, models)| {
        (active_provider.as_deref() == Some(*name)
            && active_model
                .as_deref()
                .is_none_or(|model| !models.contains(&model)))
        .then(|| models.first().copied())
        .flatten()
    });
    let mut changed = false;
    {
        let providers = doc
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::RootNotObject)?
            .entry("providers".to_owned())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::ProvidersNotObject)?;
        for (name, provider_type, models) in desired {
            let desired_models = Value::Array(
                models
                    .iter()
                    .map(|model| Value::String((*model).to_owned()))
                    .collect(),
            );
            let entry = providers
                .entry(name.to_owned())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .ok_or(ConfigureConfigPatchError::ProvidersNotObject)?;
            if entry.get("type") != Some(&Value::String(provider_type.to_owned())) {
                entry.insert("type".to_owned(), Value::String(provider_type.to_owned()));
                changed = true;
            }
            if entry.get("models") != Some(&desired_models) {
                entry.insert("models".to_owned(), desired_models);
                changed = true;
            }
        }
    }
    if let Some(default_model) = replacement_model {
        let agent = doc
            .get_mut("agent")
            .and_then(Value::as_object_mut)
            .ok_or(ConfigureConfigPatchError::AgentNotObject)?;
        agent.insert("model".to_owned(), Value::String(default_model.to_owned()));
        changed = true;
    }
    Ok(changed)
}

pub(super) fn patch_provider_doc(
    doc: &mut Value,
    provider_name: Option<&str>,
    models: Option<&[String]>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> std::result::Result<(), ConfigureConfigPatchError> {
    let active_provider = doc
        .get("agent")
        .and_then(|a| a.get("provider"))
        .and_then(|p| p.as_str())
        .map(str::to_owned);
    // Pre-provider_name Swarm versions still send the proxy token/base URL
    // without naming their provider. Recognize that legacy managed payload by
    // its credential fields and stable `openrouter` entry, so a runtime sync
    // cannot overwrite a user-selected CLI subscription provider.
    let legacy_swarm_openrouter = provider_name.is_none()
        && (api_key.is_some() || base_url.is_some())
        && doc
            .get("providers")
            .and_then(|providers| providers.get("openrouter"))
            .is_some_and(Value::is_object);
    let provider_name = provider_name
        .map(str::to_owned)
        .or_else(|| legacy_swarm_openrouter.then(|| "openrouter".to_owned()))
        .or_else(|| active_provider.clone())
        .ok_or(ConfigureConfigPatchError::MissingAgentProvider)?;
    let primary_model = models.and_then(|ms| ms.first().filter(|m| !m.trim().is_empty()).cloned());
    if active_provider.as_deref() == Some(provider_name.as_str())
        && let Some(primary) = primary_model.as_ref()
    {
        let agent = doc
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::RootNotObject)?
            .entry("agent".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::AgentNotObject)?;
        agent.insert("model".into(), Value::String(primary.clone()));
    }

    let providers = doc
        .get_mut("providers")
        .and_then(|p| p.as_object_mut())
        .ok_or(ConfigureConfigPatchError::MissingProvidersObject)?;
    let prov_entry = providers
        .get_mut(&provider_name)
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| ConfigureConfigPatchError::MissingProvider(provider_name.clone()))?;
    if let Some(ms) = models {
        prov_entry.insert(
            "models".into(),
            Value::Array(ms.iter().map(|m| Value::String(m.clone())).collect()),
        );
    }
    if let Some(k) = api_key {
        prov_entry.insert("api_key".into(), Value::String(k.to_owned()));
    }
    if let Some(u) = base_url {
        prov_entry.insert("base_url".into(), Value::String(u.to_owned()));
    }
    Ok(())
}

/// Restore Swarm's durable provider/model choice without coupling it to the
/// stable OpenRouter proxy provider patch. The selected model is moved to the
/// front (or inserted) so the ordinary loader and every future agent rebuild
/// agree with `agent.model`.
pub(super) fn patch_active_selection_doc(
    doc: &mut Value,
    provider: &str,
    model: &str,
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    let providers = doc
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or(ConfigureConfigPatchError::MissingProvidersObject)?;
    let provider_doc = providers
        .get_mut(provider)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ConfigureConfigPatchError::MissingProvider(provider.to_owned()))?;
    let models = provider_doc
        .entry("models".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| ConfigureConfigPatchError::MissingProvider(provider.to_owned()))?;
    let mut changed = false;
    if let Some(position) = models
        .iter()
        .position(|value| value.as_str() == Some(model))
    {
        if position > 0 {
            let selected = models.remove(position);
            models.insert(0, selected);
            changed = true;
        }
    } else {
        models.insert(0, Value::String(model.to_owned()));
        changed = true;
    }

    let agent = doc
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?
        .entry("agent".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::AgentNotObject)?;
    for (key, value) in [("provider", provider), ("model", model)] {
        let desired = Value::String(value.to_owned());
        if agent.get(key) != Some(&desired) {
            agent.insert(key.to_owned(), desired);
            changed = true;
        }
    }
    Ok(changed)
}

pub(super) fn patch_image_generation_doc(
    doc: &mut Value,
    provider_block: Option<(&str, &Value)>,
    image_provider: Option<&str>,
    image_model: Option<&str>,
) -> std::result::Result<(), ConfigureConfigPatchError> {
    if let Some((name, block)) = provider_block {
        let providers = doc
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::RootNotObject)?
            .entry("providers".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::ProvidersNotObject)?;
        providers.insert(name.to_owned(), block.clone());
    }

    if image_provider.is_some() || image_model.is_some() {
        let agent = doc
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::RootNotObject)?
            .entry("agent".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or(ConfigureConfigPatchError::AgentNotObject)?;
        if let Some(p) = image_provider {
            agent.insert(
                "image_generation_provider".into(),
                Value::String(p.to_owned()),
            );
        }
        if let Some(m) = image_model {
            agent.insert("image_generation_model".into(), Value::String(m.to_owned()));
        }
    }
    Ok(())
}

pub(super) fn clear_skills_doc(
    doc: &mut Value,
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    Ok(doc
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?
        .remove("skills")
        .is_some())
}

pub(super) fn set_skills_tools_doc(
    doc: &mut Value,
    tools: &[String],
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    let root = doc
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?;
    let new_tools = Value::Array(tools.iter().map(|t| Value::String(t.clone())).collect());
    let skills = root
        .entry("skills".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::SkillsNotObject)?;

    let builtin = skills
        .entry("builtin".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::SkillsBuiltinNotObject)?;
    let builtin_unchanged = builtin.get("tools") == Some(&new_tools);
    if !builtin_unchanged {
        builtin.insert("tools".to_string(), new_tools);
    }

    let allowed: std::collections::HashSet<&str> = tools.iter().map(String::as_str).collect();
    let prev = skills.remove("subagents");
    let subagents_unchanged = match prev {
        Some(Value::Array(arr)) => {
            let kept: Vec<Value> = arr
                .iter()
                .filter(|entry| {
                    entry
                        .as_object()
                        .and_then(|o| o.get("name"))
                        .and_then(Value::as_str)
                        .map(|n| allowed.contains(n))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            let unchanged = kept.len() == arr.len() && kept == arr;
            if !kept.is_empty() {
                skills.insert("subagents".to_string(), Value::Array(kept));
            }
            unchanged
        }
        Some(other) => {
            skills.insert("subagents".to_string(), other);
            true
        }
        None => true,
    };

    Ok(!(builtin_unchanged && subagents_unchanged))
}

pub(super) fn patch_mcp_servers_doc(
    doc: &mut Value,
    servers: &serde_json::Map<String, Value>,
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    let root = doc
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?;
    let new_block = Value::Object(servers.clone());
    if root.get("mcp_servers") == Some(&new_block) {
        return Ok(false);
    }
    if servers.is_empty() {
        root.remove("mcp_servers");
    } else {
        root.insert("mcp_servers".to_string(), new_block);
    }
    Ok(true)
}

pub(super) fn patch_telegram_controller_doc(
    doc: &mut Value,
    proxy: &TelegramProxyConfigure,
) -> std::result::Result<bool, ConfigureConfigPatchError> {
    let root = doc
        .as_object_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?;
    let controllers = root
        .entry("controllers".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or(ConfigureConfigPatchError::RootNotObject)?;

    let desired = serde_json::json!({
        "type": "telegram",
        "mode": "webhook",
        "allow_all_chats": true,
        "enabled": proxy.enabled,
        "proxy": {
            "base_url": proxy.base_url.clone(),
            "file_base_url": proxy.file_base_url.clone(),
            "bearer": proxy.bearer.clone(),
        }
    });

    if let Some(existing) = controllers.iter_mut().find(|entry| {
        entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "telegram")
    }) {
        if existing == &desired {
            return Ok(false);
        }
        *existing = desired;
        return Ok(true);
    }

    controllers.push(desired);
    Ok(true)
}

pub(super) async fn write_config_doc(
    path: &std::path::Path,
    doc: &Value,
) -> std::result::Result<(), ConfigureConfigPatchError> {
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(doc).map_err(ConfigureConfigPatchError::Serialize)?;
    tokio::fs::write(&tmp, &pretty)
        .await
        .map_err(ConfigureConfigPatchError::WriteTmp)?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(ConfigureConfigPatchError::Rename)?;
    Ok(())
}
