//! Convert deserialized configuration into runtime settings.
use crate::config::{
    ActiveProvider, BuiltinSkillConfig, CompactionConfig, ControllerConfig, LocalSkillConfig,
    McpAuthConfig, McpConfig, McpTransportConfig, ProviderConfig, SandboxConfig, Settings,
    SkillConfig, SubagentAgentConfig, SubagentSkillConfig,
};
use crate::secret::SecretRegistry;

use super::schema::*;
use super::secrets::resolve_secrets_in_value;

/// Convert JSON into runtime Settings.
pub(super) fn build_settings(json_root: Option<JsonRoot>, secrets: &SecretRegistry) -> Settings {
    let mut settings = Settings::default();

    let root = match json_root {
        Some(r) => r,
        None => return settings,
    };

    parse_providers(root.providers, secrets, &mut settings);
    parse_agent_settings(root.agent, &mut settings);
    parse_skills(root.skills, &mut settings);
    parse_mcp_servers(root.mcp_servers, secrets, &mut settings);
    parse_sandbox(root.sandbox, &mut settings);
    parse_workspace(root.workspace, secrets, &mut settings);
    parse_chat_history(root.chat_history, secrets, &mut settings);
    parse_transcriber(root.transcriber, &mut settings);
    parse_web_search(root.web_search, secrets, &mut settings);
    parse_controllers(root.controllers, secrets, &mut settings);

    settings
}

/// Parse named provider configurations into settings.
pub(super) fn parse_providers(
    providers: Option<std::collections::HashMap<String, JsonProviderConfig>>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let providers = match providers {
        Some(p) => p,
        None => return,
    };

    for (name, jp) in providers {
        let provider_type = match crate::llm::registry::from_str_loose(&jp.provider_type) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    provider = name.as_str(),
                    r#type = jp.provider_type.as_str(),
                    "unknown provider type — skipping"
                );
                continue;
            }
        };

        let api_key: crate::auth::Credential = match jp.api_key.as_ref() {
            Some(k) => match secrets.resolve(k) {
                Ok(resolved) => resolved,
                Err(e) => {
                    tracing::error!(
                        provider = name.as_str(),
                        error = %e,
                        "failed to resolve API key — skipping provider"
                    );
                    continue;
                }
            },
            None => crate::auth::Credential::new(String::new()),
        };

        if jp.models.is_empty() {
            tracing::error!(
                provider = name.as_str(),
                r#type = jp.provider_type.as_str(),
                "provider has no `models` configured — skipping.  \
                 Dyson no longer ships hardcoded model defaults; specify \
                 at least one model id in the provider's `models` array."
            );
            continue;
        }

        settings.providers.insert(
            name,
            ProviderConfig {
                provider_type,
                models: jp.models,
                api_key,
                base_url: jp.base_url,
            },
        );
    }
}

/// Parse agent-level settings, applying provider defaults and overrides.
pub(super) fn parse_agent_settings(agent: Option<JsonAgent>, settings: &mut Settings) {
    let agent = match agent {
        Some(a) => a,
        None => return,
    };
    let selected_provider_name = agent.provider.clone();

    // Apply the named provider's fields to agent settings.
    if let Some(provider_name) = selected_provider_name.as_deref() {
        if let Some(pc) = settings.providers.get(provider_name) {
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = pc.default_model().to_string();
            settings.agent.api_key = pc.api_key.clone();
            settings.agent.base_url = pc.base_url.clone();
        } else {
            tracing::warn!(
                provider = provider_name,
                "agent references unknown provider name"
            );
        }
    }

    // Agent-level overrides (model can override the provider's model).
    if let Some(model) = agent.model {
        settings.agent.model = model;
    }
    if let Some(limits) = agent.task_budget {
        settings.agent.task_budget = limits;
    }
    if let Some(max_iter) = agent.max_iterations {
        settings.agent.max_iterations = max_iter;
    }
    if let Some(max_retries) = agent.max_retries {
        settings.agent.max_retries = max_retries;
    }
    if let Some(cap) = agent.max_concurrent_llm_calls {
        settings.agent.max_concurrent_llm_calls = cap;
    }
    if let Some(max_tok) = agent.max_tokens {
        settings.agent.max_tokens = max_tok;
    }
    if let Some(prompt) = agent.system_prompt {
        settings.agent.system_prompt = prompt;
    }
    if let Some(compaction) = agent.compaction {
        settings.agent.compaction = parse_compaction(compaction);
    }
    if let Some(rl) = agent.rate_limit {
        settings.agent.rate_limit = Some(crate::config::RateLimitConfig {
            max_messages: rl.max_messages,
            window_secs: rl.window_secs,
        });
    }
    if agent.smartest_model.is_some() {
        settings.agent.smartest_model = agent.smartest_model;
    }
    if agent.image_generation_provider.is_some() {
        settings.agent.image_generation_provider = agent.image_generation_provider;
    }
    if agent.image_generation_model.is_some() {
        settings.agent.image_generation_model = agent.image_generation_model;
    }

    settings.active_provider = selected_provider_name
        .filter(|name| settings.providers.contains_key(name))
        .and_then(|name| ActiveProvider::new(name, settings.agent.model.clone()));
}

/// Convert a JSON compaction value into a `CompactionConfig`.
pub(super) fn parse_compaction(compaction: JsonCompaction) -> CompactionConfig {
    match compaction {
        JsonCompaction::Window(window) => CompactionConfig {
            context_window: window,
            ..Default::default()
        },
        JsonCompaction::Full {
            context_window,
            threshold_ratio,
            protect_head,
            protect_tail_tokens,
            summary_min_tokens,
            summary_max_tokens,
            summary_target_ratio,
        } => {
            let defaults = CompactionConfig::default();
            CompactionConfig {
                context_window: context_window.unwrap_or(defaults.context_window),
                threshold_ratio: threshold_ratio.unwrap_or(defaults.threshold_ratio),
                protect_head: protect_head.unwrap_or(defaults.protect_head),
                protect_tail_tokens: protect_tail_tokens.unwrap_or(defaults.protect_tail_tokens),
                summary_min_tokens: summary_min_tokens.unwrap_or(defaults.summary_min_tokens),
                summary_max_tokens: summary_max_tokens.unwrap_or(defaults.summary_max_tokens),
                summary_target_ratio: summary_target_ratio.unwrap_or(defaults.summary_target_ratio),
            }
        }
    }
}

/// Parse skill configurations (builtin, local, subagents).
pub(super) fn parse_skills(skills: Option<JsonSkills>, settings: &mut Settings) {
    let skills = match skills {
        Some(s) => s,
        None => return,
    };

    let mut skill_configs: Vec<SkillConfig> = Vec::new();

    if let Some(builtin) = skills.builtin {
        // If "tools" key is present, use exactly what's listed.
        // If "tools" key is absent, include all builtins.
        match builtin.tools {
            Some(tools) if !tools.is_empty() => {
                skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig { tools }));
            }
            Some(_) => {
                // Explicit empty array — no builtin tools.
            }
            None => {
                // No "tools" key — all builtins.
                skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig { tools: vec![] }));
            }
        }
    } else {
        // No "builtin" section — include all builtins by default.
        skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig { tools: vec![] }));
    }

    if let Some(locals) = skills.local {
        for local in locals {
            skill_configs.push(SkillConfig::Local(LocalSkillConfig {
                name: local.name,
                path: local.path,
            }));
        }
    }

    if let Some(subagents) = skills.subagents {
        let agents: Vec<SubagentAgentConfig> = subagents
            .into_iter()
            .map(|sa| SubagentAgentConfig {
                name: sa.name,
                description: sa.description,
                system_prompt: sa.system_prompt,
                provider: sa.provider,
                model: sa.model,
                max_iterations: sa.max_iterations,
                max_tokens: sa.max_tokens,
                tools: sa.tools,
                injects_protocol: None, // built-in only; not from dyson.json
            })
            .collect();

        if !agents.is_empty() {
            skill_configs.push(SkillConfig::Subagent(SubagentSkillConfig { agents }));
        }
    }

    settings.skills = skill_configs;
}

/// Parse MCP server configurations into skill configs.
pub(super) fn parse_mcp_servers(
    mcp_servers: Option<serde_json::Value>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let mut mcp_val = match mcp_servers {
        Some(v) => v,
        None => return,
    };

    resolve_secrets_in_value(&mut mcp_val, secrets);

    let servers = match mcp_val.as_object() {
        Some(s) => s,
        None => return,
    };

    for (name, server_json) in servers {
        let transport = if let Some(command) = server_json["command"].as_str() {
            let args: Vec<String> = server_json["args"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // Owned Strings are required: McpTransportConfig stores HashMap<String, String>
            // and outlives the borrowed JSON values parsed here.
            let env: std::collections::HashMap<String, String> = server_json["env"]
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            let sandbox = server_json["sandbox"].as_bool().unwrap_or(false);
            let sandbox_deny_network = server_json["sandbox_deny_network"]
                .as_bool()
                .unwrap_or(false);

            McpTransportConfig::Stdio {
                command: command.to_string(),
                args,
                env,
                sandbox,
                sandbox_deny_network,
            }
        } else if let Some(url) = server_json["url"].as_str() {
            let headers: std::collections::HashMap<String, String> = server_json["headers"]
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            let auth = parse_mcp_oauth(&server_json["auth"]);

            McpTransportConfig::Http {
                url: url.to_string(),
                headers,
                auth,
            }
        } else {
            tracing::warn!(
                server = name.as_str(),
                "MCP server has neither 'command' nor 'url' — skipping"
            );
            continue;
        };

        settings.skills.push(SkillConfig::Mcp(Box::new(McpConfig {
            name: name.clone(),
            transport,
        })));
    }
}

/// Parse optional OAuth config from an MCP server's "auth" field.
pub(super) fn parse_mcp_oauth(auth_json: &serde_json::Value) -> Option<McpAuthConfig> {
    if auth_json["type"].as_str() != Some("oauth") {
        return None;
    }
    Some(McpAuthConfig {
        client_id: auth_json["client_id"]
            .as_str()
            .map(std::string::ToString::to_string),
        client_secret: auth_json["client_secret"]
            .as_str()
            .map(std::string::ToString::to_string),
        scopes: auth_json["scopes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        redirect_uri: auth_json["redirect_uri"]
            .as_str()
            .map(std::string::ToString::to_string),
        authorization_url: auth_json["authorization_url"]
            .as_str()
            .map(std::string::ToString::to_string),
        token_url: auth_json["token_url"]
            .as_str()
            .map(std::string::ToString::to_string),
        registration_url: auth_json["registration_url"]
            .as_str()
            .map(std::string::ToString::to_string),
    })
}

/// Parse sandbox configuration.
pub(super) fn parse_sandbox(sandbox: Option<JsonSandbox>, settings: &mut Settings) {
    let sb = match sandbox {
        Some(s) => s,
        None => return,
    };

    let tool_policies = sb
        .tool_policies
        .into_iter()
        .map(|(name, jp)| {
            let config = parse_tool_policy(jp);
            (name, config)
        })
        .collect();

    settings.sandbox = SandboxConfig {
        disabled: sb.disabled,
        os_profile: sb.os_profile,
        tool_policies,
    };
}

/// Parse workspace configuration.
pub(super) fn parse_workspace(
    workspace: Option<JsonWorkspace>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let ws = match workspace {
        Some(w) => w,
        None => return,
    };

    if let Some(backend) = ws.backend {
        settings.workspace.backend = backend;
    }
    // connection_string takes priority, then fall back to legacy path.
    if let Some(ref cs) = ws.connection_string {
        if let Ok(resolved) = secrets.resolve(cs) {
            settings.workspace.connection_string = resolved;
        }
    } else if let Some(path) = ws.path {
        settings.workspace.connection_string = crate::auth::Credential::new(path);
    }
    // Memory config: merge user overrides on top of defaults.
    if let Some(mem) = ws.memory {
        if let Some(limits) = mem.limits {
            for (file, limit) in limits {
                settings.workspace.memory.limits.insert(file, limit);
            }
        }
        if let Some(factor) = mem.overflow_factor {
            settings.workspace.memory.overflow_factor = factor;
        }
        if let Some(interval) = mem.nudge_interval {
            settings.workspace.memory.nudge_interval = interval;
        }
    }
}

/// Parse chat history configuration.
pub(super) fn parse_chat_history(
    chat_history: Option<JsonChatHistory>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let ch = match chat_history {
        Some(c) => c,
        None => return,
    };

    if let Some(backend) = ch.backend {
        settings.chat_history.backend = backend;
    }
    if let Some(ref cs) = ch.connection_string
        && let Ok(resolved) = secrets.resolve(cs)
    {
        settings.chat_history.connection_string = resolved;
    }
}

/// Parse transcriber configuration.
pub(super) fn parse_transcriber(transcriber: Option<JsonTranscriber>, settings: &mut Settings) {
    let t = match transcriber {
        Some(t) => t,
        None => return,
    };

    settings.transcriber = Some(crate::config::TranscriberConfig {
        provider: t.provider.unwrap_or_else(|| "whisper-cli".into()),
        model: t.model,
    });
}

/// Parse web search configuration.
pub(super) fn parse_web_search(
    web_search: Option<JsonWebSearch>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let ws = match web_search {
        Some(w) => w,
        None => return,
    };

    let api_key = match ws.api_key {
        Some(ref sv) => match secrets.resolve(sv) {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!(error = %e, "failed to resolve web_search api_key — skipping");
                crate::auth::Credential::new(String::new())
            }
        },
        None => crate::auth::Credential::new(String::new()),
    };

    if !api_key.is_empty() || ws.base_url.is_some() {
        settings.web_search = Some(crate::config::WebSearchConfig {
            provider: ws.provider.unwrap_or_else(|| "brave".into()),
            api_key,
            base_url: ws.base_url,
        });
    }
}

/// Parse controller configurations, resolving secrets in each.
pub(super) fn parse_controllers(
    controllers: Option<Vec<serde_json::Value>>,
    secrets: &SecretRegistry,
    settings: &mut Settings,
) {
    let controllers = match controllers {
        Some(c) => c,
        None => return,
    };

    for mut ctrl_json in controllers {
        let ctrl_type = ctrl_json["type"].as_str().unwrap_or("unknown").to_string();
        resolve_secrets_in_value(&mut ctrl_json, secrets);
        settings.controllers.push(ControllerConfig {
            controller_type: ctrl_type,
            config: ctrl_json,
        });
    }
}

/// Parse a `JsonToolPolicy` into a `ToolPolicyConfig`.
///
/// File access fields can be either a simple string ("allow"/"deny")
/// or an object `{ "restrict_to": ["/path1", "/path2"] }`.
pub(super) fn parse_tool_policy(jp: JsonToolPolicy) -> crate::sandbox::policy::ToolPolicyConfig {
    use crate::sandbox::policy::{ToolPolicyConfig, ToolPolicyPathConfig};

    fn parse_path_field(val: serde_json::Value) -> Option<ToolPolicyPathConfig> {
        match val {
            serde_json::Value::String(s) => Some(ToolPolicyPathConfig::Simple(s)),
            serde_json::Value::Object(obj) => {
                if let Some(serde_json::Value::Array(arr)) = obj.get("restrict_to") {
                    let paths: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    Some(ToolPolicyPathConfig::RestrictTo(paths))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    ToolPolicyConfig {
        network: jp.network,
        file_read: jp.file_read.and_then(parse_path_field),
        file_write: jp.file_write.and_then(parse_path_field),
        process_exec: jp.process_exec,
    }
}
