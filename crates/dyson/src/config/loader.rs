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
    BuiltinSkillConfig, CompactionConfig, ControllerConfig, LocalSkillConfig, McpAuthConfig,
    McpConfig, McpTransportConfig, ProviderConfig, SandboxConfig, Settings, SkillConfig,
    SubagentAgentConfig, SubagentSkillConfig,
};
use crate::error::{DysonError, Result};
use crate::secret::{SecretRegistry, SecretValue};

// ---------------------------------------------------------------------------
// JSON file shape
// ---------------------------------------------------------------------------

/// Root of the dyson.json file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRoot {
    /// Schema version — consumed by `migrate()`, retained here so
    /// `deny_unknown_fields` doesn't reject it.
    #[serde(default)]
    #[allow(dead_code)]
    config_version: Option<u32>,
    /// Named provider configurations.
    ///
    /// ```json
    /// "providers": {
    ///   "claude": { "type": "anthropic", "models": ["claude-sonnet-4-20250514"], "api_key": "..." },
    ///   "gpt":    { "type": "openai",    "models": ["gpt-4o"] }
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
    /// Audio transcriber configuration.
    transcriber: Option<JsonTranscriber>,
    /// Web search provider configuration.
    web_search: Option<JsonWebSearch>,
}

/// The `"transcriber"` object.
///
/// ```json
/// "transcriber": {
///   "provider": "whisper-cli",
///   "model": "small"
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonTranscriber {
    /// Transcriber provider: "whisper-cli" (default).
    provider: Option<String>,
    /// Model name/size for the provider.
    model: Option<String>,
}

/// The `"web_search"` object.
///
/// ```json
/// "web_search": {
///   "provider": "brave",
///   "api_key": { "resolver": "insecure_env", "name": "BRAVE_API_KEY" }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonWebSearch {
    /// Search provider: "brave" (default).
    provider: Option<String>,
    /// API key for the search provider.  Supports secret resolution.
    api_key: Option<SecretValue>,
    /// Optional base URL override (e.g. for self-hosted SearXNG).
    base_url: Option<String>,
}

/// A single provider entry in the `"providers"` map.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonProviderConfig {
    /// Provider type: "anthropic", "openai", "claude-code", "codex".
    #[serde(rename = "type")]
    provider_type: String,
    /// Available models for this provider.
    #[serde(default)]
    models: Vec<String>,
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
#[serde(deny_unknown_fields)]
struct JsonAgent {
    /// Optional model override — takes precedence over the provider's model.
    model: Option<String>,
    max_iterations: Option<usize>,
    max_tokens: Option<u32>,
    system_prompt: Option<String>,
    /// Name of the provider from the `"providers"` map.
    provider: Option<String>,
    /// Advisor model for the advisor pattern.
    smartest_model: Option<String>,
    /// Context compaction configuration.  Accepts either:
    /// - an integer: shorthand for `{ "context_window": <value> }` with defaults
    /// - an object: full `CompactionConfig` with optional fields
    compaction: Option<JsonCompaction>,
    /// Rate limiting: `{ "max_messages": 30, "window_secs": 60 }`.
    rate_limit: Option<JsonRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRateLimit {
    max_messages: usize,
    window_secs: u64,
}

/// Flexible deserialization for the `"compaction"` field.
///
/// Accepts either a bare integer (context window shorthand) or a full object.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonCompaction {
    /// Shorthand: just the context_window size, e.g. `"compaction": 200000`.
    Window(usize),
    /// Full config object with optional fields.
    Full {
        context_window: Option<usize>,
        threshold_ratio: Option<f64>,
        protect_head: Option<usize>,
        protect_tail_tokens: Option<usize>,
        summary_min_tokens: Option<usize>,
        summary_max_tokens: Option<usize>,
        summary_target_ratio: Option<f64>,
    },
}

/// The `"skills"` object.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonSkills {
    builtin: Option<JsonBuiltinSkill>,
    local: Option<Vec<JsonLocalSkill>>,
    /// Subagent definitions — child agents spawnable as tools.
    ///
    /// ```json
    /// "subagents": [
    ///   {
    ///     "name": "research_agent",
    ///     "description": "Research specialist",
    ///     "system_prompt": "You are a research specialist.",
    ///     "provider": "gpt",
    ///     "max_iterations": 15,
    ///     "tools": ["bash", "web_search"]
    ///   }
    /// ]
    /// ```
    subagents: Option<Vec<JsonSubagent>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonLocalSkill {
    name: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonBuiltinSkill {
    tools: Option<Vec<String>>,
}

/// A single subagent definition in the `"subagents"` array.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonSubagent {
    /// Tool name (e.g., "research_agent").
    name: String,
    /// Description shown to the parent LLM.
    description: String,
    /// System prompt for the subagent.
    system_prompt: String,
    /// Provider name from the `"providers"` map.
    provider: String,
    /// Optional model override.
    model: Option<String>,
    /// Max LLM turns per invocation (default: 10).
    max_iterations: Option<usize>,
    /// Max tokens per response (default: 4096).
    max_tokens: Option<u32>,
    /// Optional tool name filter (None = inherit all parent tools).
    tools: Option<Vec<String>>,
}

/// The `"sandbox"` object.
///
/// ```json
/// "sandbox": {
///   "disabled": ["os"],
///   "os_profile": "strict"
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonSandbox {
    /// Sandbox names to disable.
    #[serde(default)]
    disabled: Vec<String>,
    /// OS sandbox profile: "default", "strict", "permissive".
    os_profile: Option<String>,
    /// Per-tool sandbox policies (tool name or glob → policy overrides).
    #[serde(default)]
    tool_policies: std::collections::HashMap<String, JsonToolPolicy>,
}

/// Per-tool policy overrides in dyson.json.
///
/// ```json
/// "web_search": {
///   "network": "allow",
///   "file_read": "deny",
///   "file_write": { "restrict_to": ["/tmp/workdir"] }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonToolPolicy {
    network: Option<String>,
    file_read: Option<serde_json::Value>,
    file_write: Option<serde_json::Value>,
    process_exec: Option<String>,
}

/// The `"workspace"` object.
///
/// Supports both new-style `backend` + `connection_string` and legacy `path`:
/// ```json
/// { "workspace": { "backend": "openclaw", "connection_string": "~/.dyson" } }
/// { "workspace": { "path": "~/.dyson" } }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonWorkspace {
    /// Backend type: "openclaw" (default).
    backend: Option<String>,
    /// Connection string (path for openclaw).  Supports secret resolution.
    connection_string: Option<SecretValue>,
    /// Legacy: plain path.  Falls back to this if connection_string is absent.
    path: Option<String>,
    /// Memory tier configuration.
    memory: Option<JsonMemory>,
}

/// The `"memory"` object inside `"workspace"`.
///
/// ```json
/// {
///   "memory": {
///     "limits": { "MEMORY.md": 2500 },
///     "overflow_factor": 1.35,
///     "nudge_interval": 5
///   }
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonMemory {
    /// Per-file soft character targets.
    limits: Option<std::collections::HashMap<String, usize>>,
    /// Multiplier that turns a soft target into a hard ceiling.
    overflow_factor: Option<f32>,
    /// Nudge interval in turns (0 = disabled).
    nudge_interval: Option<usize>,
}

/// The `"chat_history"` object.
///
/// ```json
/// { "chat_history": { "backend": "disk", "connection_string": "~/.dyson/chats" } }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// Config files are automatically migrated in-memory before parsing.
/// Old formats (e.g. inline `agent.provider`/`api_key`) are upgraded
/// to the current schema via the migration chain in `config::migrate`.
pub fn load_settings(path: Option<&Path>) -> Result<Settings> {
    let json_root = match path {
        Some(p) => {
            let content = read_config_file(p)?;
            let mut raw: serde_json::Value = serde_json::from_str(&content)?;
            if crate::config::migrate::migrate(&mut raw)? {
                write_back_config(p, &raw);
            }
            Some(serde_json::from_value::<JsonRoot>(raw)?)
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
///
/// Reads the file first, then checks the length — avoids a TOCTOU race
/// between `metadata()` and `read_to_string()` where the file could be
/// swapped between the two calls.
fn read_config_file(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| DysonError::Config(format!("cannot read config {}: {e}", path.display())))?;
    if content.len() as u64 > MAX_CONFIG_SIZE {
        return Err(DysonError::Config(format!(
            "config file {} is too large ({} bytes, max {} bytes)",
            path.display(),
            content.len(),
            MAX_CONFIG_SIZE,
        )));
    }
    Ok(content)
}

/// Try to find a dyson.json in standard locations.
///
/// Attempts to read files directly instead of checking `exists()` first,
/// avoiding a TOCTOU race where the file could be swapped between the
/// existence check and the read.
fn try_discover_config() -> Result<Option<JsonRoot>> {
    // 1. Current directory.
    let cwd_path = Path::new("dyson.json");
    if let Ok(root) = load_and_migrate(cwd_path) {
        return Ok(root);
    }

    // 2. ~/.config/dyson/dyson.json
    if let Some(home) = std::env::var_os("HOME") {
        let global_path = Path::new(&home).join(".config/dyson/dyson.json");
        if let Ok(root) = load_and_migrate(&global_path) {
            return Ok(root);
        }
    }

    Ok(None)
}

/// Read a config file, migrate it, write back if changed, and parse.
fn load_and_migrate(path: &Path) -> Result<Option<JsonRoot>> {
    let content = read_config_file(path)?;
    let mut raw: serde_json::Value = serde_json::from_str(&content)?;
    if crate::config::migrate::migrate(&mut raw)? {
        write_back_config(path, &raw);
    }
    Ok(Some(serde_json::from_value::<JsonRoot>(raw)?))
}

/// Best-effort write migrated config back to disk.
///
/// Logs a warning on failure but does not propagate errors — the in-memory
/// migration already succeeded, so the runtime can proceed.  The file will
/// be migrated again on next load if write-back fails.
///
/// On Unix, the file permissions are set to 0o600 (owner read/write only)
/// because config files may contain API keys or secret resolver references.
fn write_back_config(path: &Path, value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, format!("{json}\n")) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to write migrated config back to disk"
                );
            } else {
                // Restrict permissions — config may contain secrets.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                tracing::info!(path = %path.display(), "wrote migrated config to disk");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize migrated config");
        }
    }
}

/// Persist the user's model selection by moving the chosen model to the
/// front of the provider's `models` array in the config file.
///
/// This makes the selected model the default on next startup (since
/// `default_model()` returns `models[0]`).
///
/// Best-effort: logs a warning on failure but never crashes.
pub fn persist_model_selection(config_path: &Path, provider_name: &str, model: &str) {
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "could not read config for model persistence");
            return;
        }
    };

    let mut root: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not parse config for model persistence");
            return;
        }
    };

    // Navigate to providers.<name>.models and move the selected model to front.
    let moved = root
        .get_mut("providers")
        .and_then(|p| p.get_mut(provider_name))
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.as_array_mut())
        .map(|models| {
            if let Some(pos) = models.iter().position(|v| v.as_str() == Some(model)) {
                if pos > 0 {
                    let val = models.remove(pos);
                    models.insert(0, val);
                    true
                } else {
                    false // already first
                }
            } else {
                false
            }
        })
        .unwrap_or(false);

    // Update agent.provider and agent.model so the switched model becomes the
    // default on next startup.
    let agent_updated = root
        .get_mut("agent")
        .and_then(|a| a.as_object_mut())
        .map(|agent| {
            let mut changed = false;
            let new_provider = serde_json::Value::String(provider_name.to_string());
            if agent.get("provider") != Some(&new_provider) {
                agent.insert("provider".to_string(), new_provider);
                changed = true;
            }
            let new_model = serde_json::Value::String(model.to_string());
            if agent.get("model") != Some(&new_model) {
                agent.insert("model".to_string(), new_model);
                changed = true;
            }
            changed
        })
        .unwrap_or(false);

    if moved || agent_updated {
        write_back_config(config_path, &root);
    }
}

/// Convert JSON into runtime Settings.
fn build_settings(json_root: Option<JsonRoot>, secrets: &SecretRegistry) -> Settings {
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
fn parse_providers(
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

        let models = if jp.models.is_empty() {
            vec![
                crate::llm::registry::lookup(&provider_type)
                    .default_model
                    .into(),
            ]
        } else {
            jp.models
        };

        settings.providers.insert(
            name,
            ProviderConfig {
                provider_type,
                models,
                api_key,
                base_url: jp.base_url,
            },
        );
    }
}

/// Parse agent-level settings, applying provider defaults and overrides.
fn parse_agent_settings(agent: Option<JsonAgent>, settings: &mut Settings) {
    let agent = match agent {
        Some(a) => a,
        None => return,
    };

    // Apply the named provider's fields to agent settings.
    if let Some(ref provider_name) = agent.provider {
        if let Some(pc) = settings.providers.get(provider_name) {
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = pc.default_model().to_string();
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
}

/// Convert a JSON compaction value into a `CompactionConfig`.
fn parse_compaction(compaction: JsonCompaction) -> CompactionConfig {
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
            let mut c = CompactionConfig::default();
            if let Some(v) = context_window {
                c.context_window = v;
            }
            if let Some(v) = threshold_ratio {
                c.threshold_ratio = v;
            }
            if let Some(v) = protect_head {
                c.protect_head = v;
            }
            if let Some(v) = protect_tail_tokens {
                c.protect_tail_tokens = v;
            }
            if let Some(v) = summary_min_tokens {
                c.summary_min_tokens = v;
            }
            if let Some(v) = summary_max_tokens {
                c.summary_max_tokens = v;
            }
            if let Some(v) = summary_target_ratio {
                c.summary_target_ratio = v;
            }
            c
        }
    }
}

/// Parse skill configurations (builtin, local, subagents).
fn parse_skills(skills: Option<JsonSkills>, settings: &mut Settings) {
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
            })
            .collect();

        if !agents.is_empty() {
            skill_configs.push(SkillConfig::Subagent(SubagentSkillConfig { agents }));
        }
    }

    settings.skills = skill_configs;
}

/// Parse MCP server configurations into skill configs.
fn parse_mcp_servers(
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

            McpTransportConfig::Stdio {
                command: command.to_string(),
                args,
                env,
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
            exclude_tools: vec![],
            custom_auth: None,
        })));
    }
}

/// Parse optional OAuth config from an MCP server's "auth" field.
fn parse_mcp_oauth(auth_json: &serde_json::Value) -> Option<McpAuthConfig> {
    if auth_json["type"].as_str() != Some("oauth") {
        return None;
    }
    Some(McpAuthConfig {
        client_id: auth_json["client_id"].as_str().map(|s| s.to_string()),
        client_secret: auth_json["client_secret"].as_str().map(|s| s.to_string()),
        scopes: auth_json["scopes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        redirect_uri: auth_json["redirect_uri"].as_str().map(|s| s.to_string()),
        authorization_url: auth_json["authorization_url"]
            .as_str()
            .map(|s| s.to_string()),
        token_url: auth_json["token_url"].as_str().map(|s| s.to_string()),
        registration_url: auth_json["registration_url"]
            .as_str()
            .map(|s| s.to_string()),
    })
}

/// Parse sandbox configuration.
fn parse_sandbox(sandbox: Option<JsonSandbox>, settings: &mut Settings) {
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
fn parse_workspace(
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
fn parse_chat_history(
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
fn parse_transcriber(transcriber: Option<JsonTranscriber>, settings: &mut Settings) {
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
fn parse_web_search(
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
fn parse_controllers(
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

/// Walk a JSON value and resolve any secret references in-place.
///
/// A secret reference is a JSON object with exactly `"resolver"` and
/// `"name"` keys.  When found, it's replaced with the resolved string
/// value.  This lets controllers receive fully-resolved config without
/// knowing about the secret system.
///
/// Parse a `JsonToolPolicy` into a `ToolPolicyConfig`.
///
/// File access fields can be either a simple string ("allow"/"deny")
/// or an object `{ "restrict_to": ["/path1", "/path2"] }`.
fn parse_tool_policy(jp: JsonToolPolicy) -> crate::sandbox::policy::ToolPolicyConfig {
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
            if map.len() == 2 && map.contains_key("resolver") && map.contains_key("name") {
                let secret_val = SecretValue::Reference {
                    resolver: map["resolver"].as_str().unwrap_or("").to_string(),
                    name: map["name"].as_str().unwrap_or("").to_string(),
                };
                match secrets.resolve(&secret_val) {
                    Ok(resolved) => {
                        *value = serde_json::Value::String(resolved.expose().to_string());
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
/// have one yet, try the provider-specific env var — but ONLY if the
/// provider uses the default API endpoint (no custom `base_url`).
///
/// ## Security: no env-var fallback for custom base_url
///
/// A malicious `dyson.json` checked into a shared repo could define a
/// provider with `base_url` pointing to an attacker's server and no
/// explicit `api_key`.  Without this guard, the loader would inject the
/// victim's real API key from their environment, sending it to the
/// attacker on every request.
///
/// The fix: env-var fallback is only used when `base_url` is `None`
/// (i.e., the provider targets the official API).  Providers with a
/// custom endpoint MUST supply their own `api_key` explicitly.
fn resolve_api_keys(settings: &mut Settings, secrets: &SecretRegistry) -> Result<()> {
    // Best-effort: resolve keys for all providers in the map.
    for (name, provider) in settings.providers.iter_mut() {
        let entry = crate::llm::registry::lookup(&provider.provider_type);
        match entry.resolve_api_key(&provider.api_key, &provider.base_url, secrets, false) {
            Ok(Some(key)) => provider.api_key = key,
            Ok(None) => {}
            Err(_) => {
                tracing::debug!(
                    provider = name.as_str(),
                    "no API key for provider (not active, skipping)"
                );
            }
        }
    }

    // Required: resolve the active agent's key.
    let active_entry = crate::llm::registry::lookup(&settings.agent.provider);
    if let Some(key) = active_entry.resolve_api_key(
        &settings.agent.api_key,
        &settings.agent.base_url,
        secrets,
        true, // required — error if missing
    )? {
        settings.agent.api_key = key;
    }

    // SECURITY: warn if any provider sends API keys over plain HTTP to a
    // remote host.  Localhost is fine (Ollama, vLLM, etc.), but a remote
    // HTTP endpoint would transmit the key in cleartext.
    warn_http_with_api_key(
        &settings.agent.base_url,
        settings.agent.api_key.expose(),
        "active agent",
    );
    for (name, provider) in &settings.providers {
        warn_http_with_api_key(&provider.base_url, provider.api_key.expose(), name);
    }

    Ok(())
}

/// Emit a warning if a provider sends an API key over plain HTTP to a
/// non-localhost endpoint.  Keys over HTTP are transmitted in cleartext
/// and can be intercepted by anyone on the network path.
fn warn_http_with_api_key(base_url: &Option<String>, api_key: &str, label: &str) {
    let url = match base_url {
        Some(u) => u,
        None => return, // Default endpoint — always HTTPS.
    };
    if api_key.is_empty() {
        return; // No key to leak.
    }
    if !url.starts_with("http://") {
        return; // HTTPS or other scheme — fine.
    }
    // Allow localhost / 127.0.0.1 / [::1] — common for local model servers.
    let after_scheme = &url["http://".len()..];
    let host = after_scheme.split('/').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or(host); // strip port
    if host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1" {
        return;
    }
    tracing::warn!(
        provider = label,
        base_url = url,
        "API key will be sent over plain HTTP to a remote host — \
         this transmits the key in cleartext.  Use HTTPS or remove the api_key."
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize tests that mutate `ANTHROPIC_API_KEY`.
    static ANTHROPIC_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
                    "models": ["claude-sonnet-4-20250514"],
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
        assert_eq!(
            settings.controllers[0].config["bot_token"],
            "resolved_token"
        );
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
                    "models": ["claude-opus-4-20250514"],
                    "api_key": "sk-ant"
                },
                "gpt": {
                    "type": "openai",
                    "models": ["gpt-4o"],
                    "api_key": "sk-oai"
                },
                "local": {
                    "type": "openai",
                    "models": ["llama3"],
                    "base_url": "http://localhost:11434"
                }
            },
            "agent": { "provider": "claude" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Active provider applied to agent.
        assert_eq!(
            settings.agent.provider,
            crate::config::LlmProvider::Anthropic
        );
        assert_eq!(settings.agent.model, "claude-opus-4-20250514");
        assert_eq!(settings.agent.api_key, "sk-ant");

        // All providers in the map.
        assert_eq!(settings.providers.len(), 3);
        assert_eq!(settings.providers["gpt"].default_model(), "gpt-4o");
        assert_eq!(
            settings.providers["local"].base_url.as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn agent_model_overrides_provider() {
        let json = r#"{
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "models": ["claude-sonnet-4-20250514"],
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

        // Agent-level model overrides provider's default model.
        assert_eq!(settings.agent.model, "claude-opus-4-20250514");
        // Provider's models list is unchanged.
        assert_eq!(
            settings.providers["claude"].default_model(),
            "claude-sonnet-4-20250514"
        );
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
        assert_eq!(
            settings.agent.provider,
            crate::config::LlmProvider::Anthropic
        );
        assert_eq!(settings.agent.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn env_fallback_blocked_for_custom_base_url_provider() {
        let _guard = ANTHROPIC_KEY_LOCK.lock().unwrap();
        // SECURITY: A provider with a custom base_url must NOT get env-var
        // API keys injected — that would send the key to an untrusted endpoint.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-real-key") };
        let json = r#"{
            "providers": {
                "evil": {
                    "type": "anthropic",
                    "models": ["claude-sonnet-4-20250514"],
                    "base_url": "https://attacker.example.com/v1"
                }
            },
            "agent": { "provider": "evil" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let mut settings = build_settings(Some(root), &secrets);
        let result = resolve_api_keys(&mut settings, &secrets);

        // The active agent should fail because base_url is set without an
        // explicit api_key — env-var fallback must be refused.
        assert!(
            result.is_err(),
            "should refuse env-var fallback with custom base_url"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("base_url"),
            "error should mention base_url"
        );

        // The provider in the map should also NOT have the env key.
        assert!(
            settings.providers["evil"].api_key.is_empty(),
            "provider with custom base_url must not receive env-var key"
        );

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    #[test]
    fn env_fallback_allowed_for_default_base_url() {
        let _guard = ANTHROPIC_KEY_LOCK.lock().unwrap();
        // When there's no custom base_url, env-var fallback works normally.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-legit-key") };
        let json = r#"{
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "models": ["claude-sonnet-4-20250514"]
                }
            },
            "agent": { "provider": "claude" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let mut settings = build_settings(Some(root), &secrets);
        let result = resolve_api_keys(&mut settings, &secrets);

        assert!(
            result.is_ok(),
            "env-var fallback should work without custom base_url"
        );
        assert_eq!(settings.agent.api_key, "sk-legit-key");
        assert_eq!(settings.providers["claude"].api_key, "sk-legit-key");

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    #[test]
    fn explicit_key_with_custom_base_url_works() {
        // A provider with both an explicit api_key AND a custom base_url
        // should work fine — the user chose to send that key there.
        let json = r#"{
            "providers": {
                "proxy": {
                    "type": "anthropic",
                    "models": ["claude-sonnet-4-20250514"],
                    "api_key": "sk-explicit-for-proxy",
                    "base_url": "https://my-proxy.example.com/v1"
                }
            },
            "agent": { "provider": "proxy" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let mut settings = build_settings(Some(root), &secrets);
        let result = resolve_api_keys(&mut settings, &secrets);

        assert!(result.is_ok());
        assert_eq!(settings.agent.api_key, "sk-explicit-for-proxy");
        assert_eq!(
            settings.agent.base_url.as_deref(),
            Some("https://my-proxy.example.com/v1")
        );
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

    #[test]
    fn unresolvable_secret_skips_provider() {
        // A provider with a secret reference that can't be resolved should
        // be skipped entirely — not silently defaulted to an empty key.
        let json = r#"{
            "providers": {
                "bad": {
                    "type": "anthropic",
                    "api_key": { "resolver": "insecure_env", "name": "DYSON_NONEXISTENT_VAR_12345" }
                }
            },
            "agent": { "provider": "bad" }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // The provider should have been skipped — not inserted with empty key.
        assert!(
            !settings.providers.contains_key("bad"),
            "provider with unresolvable secret should be skipped"
        );
    }

    // -----------------------------------------------------------------------
    // Subagent config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_subagent_config() {
        let json = r#"{
            "providers": {
                "gpt": {
                    "type": "openai",
                    "models": ["gpt-4o"],
                    "api_key": "sk-test"
                }
            },
            "skills": {
                "builtin": {},
                "subagents": [
                    {
                        "name": "research_agent",
                        "description": "Research specialist",
                        "system_prompt": "You are a researcher.",
                        "provider": "gpt",
                        "max_iterations": 15,
                        "max_tokens": 4096,
                        "tools": ["bash", "web_search"]
                    },
                    {
                        "name": "code_agent",
                        "description": "Code reviewer",
                        "system_prompt": "You review code.",
                        "provider": "gpt"
                    }
                ]
            }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        // Should have builtin + subagent skill configs.
        let subagent_configs: Vec<_> = settings
            .skills
            .iter()
            .filter_map(|s| match s {
                SkillConfig::Subagent(cfg) => Some(cfg),
                _ => None,
            })
            .collect();
        assert_eq!(subagent_configs.len(), 1);

        let agents = &subagent_configs[0].agents;
        assert_eq!(agents.len(), 2);

        assert_eq!(agents[0].name, "research_agent");
        assert_eq!(agents[0].description, "Research specialist");
        assert_eq!(agents[0].provider, "gpt");
        assert_eq!(agents[0].max_iterations, Some(15));
        assert_eq!(agents[0].max_tokens, Some(4096));
        assert_eq!(
            agents[0].tools,
            Some(vec!["bash".to_string(), "web_search".to_string()])
        );

        assert_eq!(agents[1].name, "code_agent");
        assert_eq!(agents[1].model, None);
        assert_eq!(agents[1].max_iterations, None);
        assert_eq!(agents[1].tools, None);
    }

    #[test]
    fn parse_subagent_minimal() {
        // All required fields, no optional ones.
        let json = r#"{
            "skills": {
                "subagents": [
                    {
                        "name": "helper",
                        "description": "A helpful agent",
                        "system_prompt": "Help the user.",
                        "provider": "claude"
                    }
                ]
            }
        }"#;
        let root: JsonRoot = serde_json::from_str(json).unwrap();
        let secrets = SecretRegistry::default();
        let settings = build_settings(Some(root), &secrets);

        let subagent_configs: Vec<_> = settings
            .skills
            .iter()
            .filter_map(|s| match s {
                SkillConfig::Subagent(cfg) => Some(cfg),
                _ => None,
            })
            .collect();
        assert_eq!(subagent_configs.len(), 1);
        assert_eq!(subagent_configs[0].agents[0].name, "helper");
        assert_eq!(subagent_configs[0].agents[0].model, None);
    }
}
