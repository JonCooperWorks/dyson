// ===========================================================================
// JSON config loader — parses dyson.json into runtime Settings.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Reads a `dyson.json` file and produces a `Settings` struct.  JSON is
//   Dyson's config format because it handles nesting naturally — each
//   controller declares its own fields inside its object, no flat-struct
//   workarounds needed.
//
// Config file discovery:
//   1. Explicit path from --config CLI flag
//   2. ./dyson.json in the current working directory
//   3. ~/.config/dyson/dyson.json (XDG-style global config)
//   4. No file found → use defaults
//
// Secret resolution:
//   Any config value that's a secret can be either:
//   - A literal string: `"bot_token": "123:ABC"`
//   - A resolver reference: `"bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" }`
//
//   The `SecretValue` enum deserializes both forms via `#[serde(untagged)]`.
//   Resolution happens during config loading — the runtime Settings struct
//   has plain `String` fields with the resolved values.
//
// Controller config:
//   Each controller is a JSON object with a `"type"` field.  Everything
//   else in the object is controller-specific and passed through as a
//   raw `serde_json::Value`.  The controller implementation parses its
//   own fields.  This means adding a new controller type never touches
//   the config loader.
//
//   ```json
//   {
//     "controllers": [
//       { "type": "terminal" },
//       {
//         "type": "telegram",
//         "bot_token": { "resolver": "insecure_env", "name": "TELEGRAM_API_KEY" },
//         "allowed_chat_ids": [123456789]
//       },
//       {
//         "type": "discord",
//         "guild_id": "123456",
//         "token": { "resolver": "vault", "name": "secret/discord-token" }
//       }
//     ]
//   }
//   ```
// ===========================================================================

use std::path::Path;

use serde::Deserialize;

use crate::config::{
    BuiltinSkillConfig, ControllerConfig, DockerSandboxConfig, LlmProvider, McpConfig,
    McpTransportConfig, ProviderConfig, SandboxConfig, Settings, SkillConfig,
};
use crate::error::{DysonError, Result};
use crate::secret::{SecretRegistry, SecretValue};

// ---------------------------------------------------------------------------
// JSON file shape
// ---------------------------------------------------------------------------

/// Root of the dyson.json file.
#[derive(Debug, Deserialize)]
struct JsonRoot {
    /// Named provider configurations.
    ///
    /// ```json
    /// "providers": {
    ///   "claude": { "type": "anthropic", "model": "claude-sonnet-4-20250514", "api_key": "..." },
    ///   "gpt":    { "type": "openai",    "model": "gpt-4o" }
    /// }
    /// ```
    providers: Option<std::collections::HashMap<String, JsonProviderConfig>>,
    agent: Option<JsonAgent>,
    skills: Option<JsonSkills>,
    controllers: Option<Vec<serde_json::Value>>,
    sandbox: Option<JsonSandbox>,
    workspace: Option<JsonWorkspace>,
    chat_history: Option<JsonChatHistory>,
    /// MCP servers — each becomes a Skill that provides tools.
    ///
    /// ```json
    /// "mcp_servers": {
    ///   "github": { "command": "npx", "args": [...], "env": {...} },
    ///   "postgres": { "command": "npx", "args": [...] }
    /// }
    /// ```
    mcp_servers: Option<serde_json::Value>,
}

/// A single provider entry in the `"providers"` map.
#[derive(Debug, Deserialize)]
struct JsonProviderConfig {
    /// Provider type: "anthropic", "openai", "claude-code", "codex".
    #[serde(rename = "type")]
    provider_type: String,
    /// Model identifier (optional, defaults per provider type).
    model: Option<String>,
    /// API key — literal string or secret resolver reference.
    api_key: Option<SecretValue>,
    /// Base URL override.
    base_url: Option<String>,
}

/// The `"agent"` object.
///
/// Provider-specific fields (api_key, base_url) live in the `"providers"`
/// map.  The agent references a provider by name.  `model` can optionally
/// override the provider's model.
#[derive(Debug, Deserialize)]
struct JsonAgent {
    /// Optional model override — takes precedence over the provider's model.
    model: Option<String>,
    max_iterations: Option<usize>,
    max_tokens: Option<u32>,
    system_prompt: Option<String>,
    /// Name of the provider from the `"providers"` map.
    provider: Option<String>,
}

/// The `"skills"` object.
#[derive(Debug, Deserialize)]
struct JsonSkills {
    builtin: Option<JsonBuiltinSkill>,
}

#[derive(Debug, Deserialize)]
struct JsonBuiltinSkill {
    tools: Option<Vec<String>>,
}

/// The `"sandbox"` object.
///
/// ```json
/// "sandbox": {
///   "disabled": ["docker"],
///   "docker": { "container": "dyson-sandbox" }
/// }
/// ```
#[derive(Debug, Deserialize)]
struct JsonSandbox {
    /// Sandbox names to disable.
    #[serde(default)]
    disabled: Vec<String>,
    /// OS sandbox profile: "default", "strict", "permissive".
    os_profile: Option<String>,
    /// Docker sandbox config.
    docker: Option<JsonDockerSandbox>,
}

#[derive(Debug, Deserialize)]
struct JsonDockerSandbox {
    container: String,
}

/// The `"workspace"` object.
///
/// Supports both new-style `backend` + `connection_string` and legacy `path`:
/// ```json
/// { "workspace": { "backend": "openclaw", "connection_string": "~/.dyson" } }
/// { "workspace": { "path": "~/.dyson" } }
/// ```
#[derive(Debug, Deserialize)]
struct JsonWorkspace {
    /// Backend type: "openclaw" (default).
    backend: Option<String>,
    /// Connection string (path for openclaw).  Supports secret resolution.
    connection_string: Option<SecretValue>,
    /// Legacy: plain path.  Falls back to this if connection_string is absent.
    path: Option<String>,
}

/// The `"chat_history"` object.
///
/// ```json
/// { "chat_history": { "backend": "disk", "connection_string": "~/.dyson/chats" } }
/// ```
#[derive(Debug, Deserialize)]
struct JsonChatHistory {
    /// Backend type: "disk" (default).
    backend: Option<String>,
    /// Connection string (directory path for disk).  Supports secret resolution.
    connection_string: Option<SecretValue>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Maximum config file size (1 MB). Prevents DoS from huge config files.
const MAX_CONFIG_SIZE: u64 = 1024 * 1024;

/// Load settings from a dyson.json file, falling back to defaults.
///
/// ## Resolution order
///
/// 1. If `path` is `Some`, load that exact file (error if missing).
/// 2. Try `./dyson.json` in the current directory.
/// 3. Try `~/.config/dyson/dyson.json`.
/// 4. No file found → use built-in defaults.
pub fn load_settings(path: Option<&Path>) -> Result<Settings> {
    let json_root = match path {
        Some(p) => {
            let content = read_config_file(p)?;
            Some(serde_json::from_str::<JsonRoot>(&content)?)
        }
        None => try_discover_config()?,
    };

    let secrets = SecretRegistry::default();
    let mut settings = build_settings(json_root, &secrets);

    resolve_api_keys(&mut settings, &secrets)?;

    Ok(settings)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a config file with a size limit to prevent DoS.
fn read_config_file(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        DysonError::Config(format!("cannot read config {}: {e}", path.display()))
    })?;
    if metadata.len() > MAX_CONFIG_SIZE {
        return Err(DysonError::Config(format!(
            "config file {} is too large ({} bytes, max {} bytes)",
            path.display(),
            metadata.len(),
            MAX_CONFIG_SIZE,
        )));
    }
    std::fs::read_to_string(path).map_err(|e| {
        DysonError::Config(format!("cannot read config {}: {e}", path.display()))
    })
}

/// Try to find a dyson.json in standard locations.
fn try_discover_config() -> Result<Option<JsonRoot>> {
    // 1. Current directory.
    let cwd_path = Path::new("dyson.json");
    if cwd_path.exists() {
        let content = read_config_file(cwd_path)?;
        return Ok(Some(serde_json::from_str::<JsonRoot>(&content)?));
    }

    // 2. ~/.config/dyson/dyson.json
    if let Some(home) = std::env::var_os("HOME") {
        let global_path = Path::new(&home).join(".config/dyson/dyson.json");
        if global_path.exists() {
            let content = read_config_file(&global_path)?;
            return Ok(Some(serde_json::from_str::<JsonRoot>(&content)?));
        }
    }

    Ok(None)
}

/// Convert JSON into runtime Settings.
fn build_settings(json_root: Option<JsonRoot>, secrets: &SecretRegistry) -> Settings {
    let mut settings = Settings::default();

    let root = match json_root {
        Some(r) => r,
        None => return settings,
    };

    // -- Providers --
    //
    // Parse all named providers first so the agent section can reference them.
    if let Some(providers) = root.providers {
        for (name, jp) in providers {
            let provider_type = match LlmProvider::from_str_loose(&jp.provider_type) {
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

            let api_key = jp
                .api_key
                .as_ref()
                .and_then(|k| secrets.resolve(k).ok())
                .unwrap_or_default();

            let model = jp.model.unwrap_or_else(|| {
                match provider_type {
                    LlmProvider::Anthropic => "claude-sonnet-4-20250514",
                    LlmProvider::OpenAi => "gpt-4o",
                    LlmProvider::ClaudeCode => "claude-sonnet-4-20250514",
                    LlmProvider::Codex => "codex",
                }
                .into()
            });

            settings.providers.insert(
                name,
                ProviderConfig {
                    provider_type,
                    model,
                    api_key,
                    base_url: jp.base_url,
                },
            );
        }
    }

    // -- Agent settings --
    if let Some(agent) = root.agent {
        // Apply the named provider's fields to agent settings.
        if let Some(ref provider_name) = agent.provider {
            if let Some(pc) = settings.providers.get(provider_name) {
                settings.agent.provider = pc.provider_type.clone();
                settings.agent.model = pc.model.clone();
                settings.agent.api_key = pc.api_key.clone();
                settings.agent.base_url = pc.base_url.clone();
            } else {
                tracing::warn!(
                    provider = provider_name.as_str(),
                    "agent references unknown provider name"
                );
            }
        }

        // Agent-level overrides (model can override the provider's model).
        if let Some(model) = agent.model {
            settings.agent.model = model;
        }
        if let Some(max_iter) = agent.max_iterations {
            settings.agent.max_iterations = max_iter;
        }
        if let Some(max_tok) = agent.max_tokens {
            settings.agent.max_tokens = max_tok;
        }
        if let Some(prompt) = agent.system_prompt {
            settings.agent.system_prompt = prompt;
        }
    }

    // -- Skills --
    if let Some(skills) = root.skills {
        let mut skill_configs: Vec<SkillConfig> = Vec::new();

        if let Some(builtin) = skills.builtin {
            // If "tools" key is present, use exactly what's listed.
            // If "tools" key is absent, include all builtins.
            // This means `"tools": []` = no tools, `"tools": ["bash"]` = just bash,
            // and no "builtin" key at all = all tools.
            match builtin.tools {
                Some(tools) if !tools.is_empty() => {
                    skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig { tools }));
                }
                Some(_) => {
                    // Explicit empty array — no builtin tools.
                }
                None => {
                    // No "tools" key — all builtins.
                    skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig {
                        tools: vec![],
                    }));
                }
            }
        } else {
            // No "builtin" section — include all builtins by default.
            skill_configs.push(SkillConfig::Builtin(BuiltinSkillConfig {
                tools: vec![],
            }));
        }

        settings.skills = skill_configs;
    }

    // -- MCP servers --
    //
    // Each key in "mcp_servers" is a server name.  The value is an object
    // with "command" + "args" + "env" (stdio) or "url" + "headers" (HTTP).
    // Each becomes a SkillConfig::Mcp that gets loaded as an McpSkill.
    if let Some(mut mcp_val) = root.mcp_servers {
        // Resolve secrets in the entire mcp_servers block (API keys in
        // headers, env vars, etc.).
        resolve_secrets_in_value(&mut mcp_val, secrets);

        if let Some(servers) = mcp_val.as_object() {
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

                    let env: std::collections::HashMap<String, String> = server_json["env"]
                        .as_object()
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| {
                                    v.as_str().map(|s| (k.clone(), s.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    McpTransportConfig::Stdio {
                        command: command.to_string(),
                        args,
                        env,
                    }
                } else if let Some(url) = server_json["url"].as_str() {
                    let headers: std::collections::HashMap<String, String> = server_json
                        ["headers"]
                        .as_object()
                        .map(|obj| {
                            obj.iter()
                                .filter_map(|(k, v)| {
                                    v.as_str().map(|s| (k.clone(), s.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    McpTransportConfig::Http {
                        url: url.to_string(),
                        headers,
                    }
                } else {
                    tracing::warn!(
                        server = name.as_str(),
                        "MCP server has neither 'command' nor 'url' — skipping"
                    );
                    continue;
                };

                settings.skills.push(SkillConfig::Mcp(McpConfig {
                    name: name.clone(),
                    transport,
                }));
            }
        }
    }

    // -- Sandbox --
    if let Some(sb) = root.sandbox {
        settings.sandbox = SandboxConfig {
            disabled: sb.disabled,
            os_profile: sb.os_profile,
            docker: sb.docker.map(|d| DockerSandboxConfig {
                container: d.container,
            }),
        };
    }

    // -- Workspace --
    if let Some(ws) = root.workspace {
        if let Some(backend) = ws.backend {
            settings.workspace.backend = backend;
        }
        // connection_string takes priority, then fall back to legacy path.
        if let Some(ref cs) = ws.connection_string {
            if let Ok(resolved) = secrets.resolve(cs) {
                settings.workspace.connection_string = resolved;
            }
        } else if let Some(path) = ws.path {
            settings.workspace.connection_string = path;
        }
    }

    // -- Chat history --
    if let Some(ch) = root.chat_history {
        if let Some(backend) = ch.backend {
            settings.chat_history.backend = backend;
        }
        if let Some(ref cs) = ch.connection_string {
            if let Ok(resolved) = secrets.resolve(cs) {
                settings.chat_history.connection_string = resolved;
            }
        }
    }

    // -- Controllers --
    //
    // Each controller is a JSON object with a "type" field.  We extract
    // the type and pass the entire object as a raw Value for the
    // controller implementation to parse.
    //
    // Secret values inside the controller config (like bot_token) are
    // resolved HERE before the controller sees them.  We walk the JSON
    // object and resolve any { "resolver": ..., "name": ... } values.
    if let Some(controllers) = root.controllers {
        for mut ctrl_json in controllers {
            let ctrl_type = ctrl_json["type"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();

            // Resolve secrets in the controller config.
            resolve_secrets_in_value(&mut ctrl_json, secrets);

            settings.controllers.push(ControllerConfig {
                controller_type: ctrl_type,
                config: ctrl_json,
            });
        }
    }

    settings
}

/// Walk a JSON value and resolve any secret references in-place.
///
/// A secret reference is a JSON object with exactly `"resolver"` and
/// `"name"` keys.  When found, it's replaced with the resolved string
/// value.  This lets controllers receive fully-resolved config without
/// knowing about the secret system.
///
/// ```json
/// // Before:
/// { "bot_token": { "resolver": "insecure_env", "name": "MY_TOKEN" } }
///
/// // After (if MY_TOKEN=abc123):
/// { "bot_token": "abc123" }
/// ```
fn resolve_secrets_in_value(value: &mut serde_json::Value, secrets: &SecretRegistry) {
    match value {
        serde_json::Value::Object(map) => {
            // Check if THIS object is a secret reference.
            if map.len() == 2
                && map.contains_key("resolver")
                && map.contains_key("name")
            {
                let secret_val = SecretValue::Reference {
                    resolver: map["resolver"].as_str().unwrap_or("").to_string(),
                    name: map["name"].as_str().unwrap_or("").to_string(),
                };
                match secrets.resolve(&secret_val) {
                    Ok(resolved) => {
                        *value = serde_json::Value::String(resolved);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "secret resolution failed in controller config");
                        return;
                    }
                }
            }

            // Otherwise, recurse into child values.
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(v) = map.get_mut(&key) {
                    resolve_secrets_in_value(v, secrets);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                resolve_secrets_in_value(item, secrets);
            }
        }
        // Strings, numbers, bools, null — no resolution needed.
        _ => {}
    }
}

/// Resolve API keys for all providers and the active agent.
///
/// For each provider that needs an API key (Anthropic, OpenAI) but doesn't
/// have one yet, try the provider-specific env var.  The active agent's key
/// is required (errors if missing); non-active providers are best-effort.
fn resolve_api_keys(settings: &mut Settings, secrets: &SecretRegistry) -> Result<()> {
    // Best-effort: resolve keys for all providers in the map.
    for (name, provider) in settings.providers.iter_mut() {
        if provider.provider_type == LlmProvider::ClaudeCode
            || provider.provider_type == LlmProvider::Codex
        {
            continue;
        }
        if !provider.api_key.is_empty() {
            continue;
        }
        let env_var = match provider.provider_type {
            LlmProvider::Anthropic => "ANTHROPIC_API_KEY",
            LlmProvider::OpenAi => "OPENAI_API_KEY",
            LlmProvider::ClaudeCode | LlmProvider::Codex => continue,
        };
        match secrets.resolve_or_env_fallback(&SecretValue::Literal(String::new()), env_var) {
            Ok(key) => provider.api_key = key,
            Err(_) => {
                tracing::debug!(provider = name.as_str(), "no API key for provider (not active, skipping)");
            }
        }
    }

    // Required: resolve the active agent's key.
    if settings.agent.provider == LlmProvider::ClaudeCode
        || settings.agent.provider == LlmProvider::Codex
    {
        return Ok(());
    }

    if !settings.agent.api_key.is_empty() {
        return Ok(());
    }

    let env_fallback = match settings.agent.provider {
        LlmProvider::Anthropic => "ANTHROPIC_API_KEY",
        LlmProvider::OpenAi => "OPENAI_API_KEY",
        LlmProvider::ClaudeCode | LlmProvider::Codex => unreachable!(),
    };

    settings.agent.api_key = secrets.resolve_or_env_fallback(
        &SecretValue::Literal(String::new()),
        env_fallback,
    )?;

    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_json() {
        let json = r#"{ "agent": { "model": "claude-opus-4-20250514" } }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        assert_eq!(root.agent.unwrap().model.unwrap(), "claude-opus-4-20250514");
    }

    #[test]
    fn parse_full_json() {
        let json = r#"{
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "model": "claude-sonnet-4-20250514",
                    "api_key": "sk-test"
                }
            },
            "agent": {
                "provider": "claude",
                "max_iterations": 50,
                "max_tokens": 16384
            },
            "skills": {
                "builtin": { "tools": ["bash"] }
            },
            "controllers": [
                { "type": "terminal" },
                { "type": "telegram", "bot_token": "test-token", "allowed_chat_ids": [123] }
            ]
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        assert_eq!(settings.agent.model, "claude-sonnet-4-20250514");
        assert_eq!(settings.agent.max_iterations, 50);
        assert_eq!(settings.agent.api_key, "sk-test");
        assert_eq!(
            settings.agent.provider,
            crate::config::LlmProvider::Anthropic
        );
        assert_eq!(settings.providers.len(), 1);
        assert!(settings.providers.contains_key("claude"));
        assert_eq!(settings.controllers.len(), 2);
        assert_eq!(settings.controllers[0].controller_type, "terminal");
        assert_eq!(settings.controllers[1].controller_type, "telegram");
        // bot_token should be resolved as a literal string in the config blob.
        assert_eq!(settings.controllers[1].config["bot_token"], "test-token");
    }

    #[test]
    fn defaults_when_no_config() {
        let secrets = SecretRegistry::default();
        let settings = build_settings(None, &secrets);
        assert_eq!(settings.agent.model, "claude-sonnet-4-20250514");
        assert_eq!(settings.agent.max_iterations, 20);
        assert!(!settings.skills.is_empty());
    }

    #[test]
    fn secret_resolution_in_controller() {
        unsafe { std::env::set_var("DYSON_JSON_TEST_TOKEN", "resolved_token") };
        let json = r#"{
            "controllers": [
                {
                    "type": "telegram",
                    "bot_token": { "resolver": "insecure_env", "name": "DYSON_JSON_TEST_TOKEN" }
                }
            ]
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // The secret reference should be resolved to the plain string.
        assert_eq!(settings.controllers[0].config["bot_token"], "resolved_token");
        unsafe { std::env::remove_var("DYSON_JSON_TEST_TOKEN") };
    }

    #[test]
    fn literal_and_reference_both_work() {
        unsafe { std::env::set_var("DYSON_JSON_TEST_2", "from_env") };
        let json = r#"{
            "providers": {
                "test": {
                    "type": "anthropic",
                    "api_key": { "resolver": "insecure_env", "name": "DYSON_JSON_TEST_2" }
                }
            },
            "agent": {
                "provider": "test"
            }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);
        assert_eq!(settings.agent.api_key, "from_env");
        assert_eq!(settings.providers["test"].api_key, "from_env");
        unsafe { std::env::remove_var("DYSON_JSON_TEST_2") };
    }

    #[test]
    fn multiple_providers_parsed() {
        let json = r#"{
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "model": "claude-opus-4-20250514",
                    "api_key": "sk-ant"
                },
                "gpt": {
                    "type": "openai",
                    "model": "gpt-4o",
                    "api_key": "sk-oai"
                },
                "local": {
                    "type": "openai",
                    "model": "llama3",
                    "base_url": "http://localhost:11434"
                }
            },
            "agent": { "provider": "claude" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Active provider applied to agent.
        assert_eq!(settings.agent.provider, crate::config::LlmProvider::Anthropic);
        assert_eq!(settings.agent.model, "claude-opus-4-20250514");
        assert_eq!(settings.agent.api_key, "sk-ant");

        // All providers in the map.
        assert_eq!(settings.providers.len(), 3);
        assert_eq!(settings.providers["gpt"].model, "gpt-4o");
        assert_eq!(settings.providers["local"].base_url.as_deref(), Some("http://localhost:11434"));
    }

    #[test]
    fn agent_model_overrides_provider() {
        let json = r#"{
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "model": "claude-sonnet-4-20250514",
                    "api_key": "sk-test"
                }
            },
            "agent": {
                "provider": "claude",
                "model": "claude-opus-4-20250514"
            }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Agent-level model overrides provider's model.
        assert_eq!(settings.agent.model, "claude-opus-4-20250514");
        // Provider's model is unchanged in the map.
        assert_eq!(settings.providers["claude"].model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn unknown_provider_name_warns() {
        let json = r#"{
            "providers": {
                "claude": { "type": "anthropic", "api_key": "sk-test" }
            },
            "agent": { "provider": "nonexistent" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Falls back to defaults when provider name not found.
        assert_eq!(settings.agent.provider, crate::config::LlmProvider::Anthropic);
        assert_eq!(settings.agent.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn controller_config_is_opaque() {
        let json = r#"{
            "controllers": [
                {
                    "type": "discord",
                    "guild_id": "123456",
                    "channel": "general",
                    "token": "my-discord-token"
                }
            ]
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Unknown controller types pass through fine — the config blob
        // is preserved for the controller to parse.
        assert_eq!(settings.controllers[0].controller_type, "discord");
        assert_eq!(settings.controllers[0].config["guild_id"], "123456");
        assert_eq!(settings.controllers[0].config["channel"], "general");
    }
}
