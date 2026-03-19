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
    BuiltinSkillConfig, ControllerConfig, DockerSandboxConfig, McpConfig, McpTransportConfig,
    SandboxConfig, Settings, SkillConfig,
};
use crate::error::{DysonError, Result};
use crate::secret::{SecretRegistry, SecretValue};

// ---------------------------------------------------------------------------
// JSON file shape
// ---------------------------------------------------------------------------

/// Root of the dyson.json file.
#[derive(Debug, Deserialize)]
struct JsonRoot {
    agent: Option<JsonAgent>,
    skills: Option<JsonSkills>,
    controllers: Option<Vec<serde_json::Value>>,
    sandbox: Option<JsonSandbox>,
    workspace: Option<JsonWorkspace>,
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

/// The `"agent"` object.
#[derive(Debug, Deserialize)]
struct JsonAgent {
    model: Option<String>,
    max_iterations: Option<usize>,
    max_tokens: Option<u32>,
    system_prompt: Option<String>,
    api_key: Option<SecretValue>,
    provider: Option<String>,
    base_url: Option<String>,
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
#[derive(Debug, Deserialize)]
struct JsonWorkspace {
    /// Path to the workspace directory.
    path: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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
            let content = std::fs::read_to_string(p).map_err(|e| {
                DysonError::Config(format!("cannot read config {}: {e}", p.display()))
            })?;
            Some(serde_json::from_str::<JsonRoot>(&content)?)
        }
        None => try_discover_config()?,
    };

    let secrets = SecretRegistry::default();
    let mut settings = build_settings(json_root, &secrets);

    resolve_api_key(&mut settings, &secrets)?;

    Ok(settings)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Try to find a dyson.json in standard locations.
fn try_discover_config() -> Result<Option<JsonRoot>> {
    // 1. Current directory.
    let cwd_path = Path::new("dyson.json");
    if cwd_path.exists() {
        let content = std::fs::read_to_string(cwd_path)?;
        return Ok(Some(serde_json::from_str::<JsonRoot>(&content)?));
    }

    // 2. ~/.config/dyson/dyson.json
    if let Some(home) = std::env::var_os("HOME") {
        let global_path = Path::new(&home).join(".config/dyson/dyson.json");
        if global_path.exists() {
            let content = std::fs::read_to_string(&global_path)?;
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

    // -- Agent settings --
    if let Some(agent) = root.agent {
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
        if let Some(ref key) = agent.api_key {
            if let Ok(resolved) = secrets.resolve(key) {
                settings.agent.api_key = resolved;
            }
        }
        if let Some(provider) = agent.provider {
            settings.agent.provider = match provider.to_lowercase().as_str() {
                "anthropic" => crate::config::LlmProvider::Anthropic,
                "openai" | "gpt" | "codex" => crate::config::LlmProvider::OpenAi,
                "claude-code" | "claude_code" | "cc" => crate::config::LlmProvider::ClaudeCode,
                other => {
                    tracing::warn!(provider = other, "unknown provider, defaulting to anthropic");
                    crate::config::LlmProvider::Anthropic
                }
            };
        }
        if let Some(base_url) = agent.base_url {
            settings.agent.base_url = Some(base_url);
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
        settings.workspace_path = ws.path;
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

/// Ensure we have an API key (unless using Claude Code).
fn resolve_api_key(settings: &mut Settings, secrets: &SecretRegistry) -> Result<()> {
    if settings.agent.provider == crate::config::LlmProvider::ClaudeCode {
        return Ok(());
    }

    if !settings.agent.api_key.is_empty() {
        return Ok(());
    }

    // Fall back to provider-specific env var.
    let env_fallback = match settings.agent.provider {
        crate::config::LlmProvider::Anthropic => "ANTHROPIC_API_KEY",
        crate::config::LlmProvider::OpenAi => "OPENAI_API_KEY",
        crate::config::LlmProvider::ClaudeCode => unreachable!(),
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
            "agent": {
                "model": "claude-sonnet-4-20250514",
                "max_iterations": 50,
                "max_tokens": 16384,
                "api_key": "sk-test",
                "provider": "anthropic"
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
            "agent": {
                "api_key": { "resolver": "insecure_env", "name": "DYSON_JSON_TEST_2" }
            }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);
        assert_eq!(settings.agent.api_key, "from_env");
        unsafe { std::env::remove_var("DYSON_JSON_TEST_2") };
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
