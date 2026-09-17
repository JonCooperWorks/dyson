use super::*;
use super::{files::*, schema::*, secrets::*};
use crate::config::{SkillConfig, SubagentAgentConfig, SubagentSkillConfig};

/// Mutex to serialize tests that mutate `ANTHROPIC_API_KEY`.
static ANTHROPIC_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn parse_minimal_json() {
    let json = r#"{ "agent": { "model": "claude-opus-4-20250514" } }"#;
    let root: JsonRoot = serde_json::from_str(json).unwrap();
    assert_eq!(root.agent.unwrap().model.unwrap(), "claude-opus-4-20250514");
}

/// Regression for the dyson-in-cube `warmup-placeholder` bug:
/// swarm pushes `http://192.168.0.1:8080/llm/openrouter` as the
/// per-cube proxy_base (the cube-dev gateway IP — host-local, no
/// hairpin to the host's public IP).  Before the fix, the loader's
/// `reject_http_with_api_key` saw `http://` + non-localhost host +
/// non-empty api_key and threw.  The hot-reload path swallowed the
/// error, the registry never picked up the patched values, and
/// the agent kept calling api.openai.com with `warmup-placeholder`
/// for the chat's lifetime.
///
/// All addresses below stay on the local network (loopback +
/// RFC1918 + link-local + Tailscale 100.64/10) — plain HTTP is
/// fine because the api_key never crosses an untrusted hop.
#[test]
fn http_with_api_key_allowed_for_local_network_hosts() {
    for host in [
        "localhost",
        "127.0.0.1",
        "[::1]",
        "10.0.0.1",
        "172.16.5.10",
        "172.31.255.254",
        "192.168.0.1",
        "192.168.50.50",
        "169.254.68.5",
        "100.64.1.1",
        "100.118.7.88", // tailnet
    ] {
        let url = format!("http://{host}:8080/llm/openrouter");
        assert!(
            reject_http_with_api_key(&Some(url.clone()), "key", "openrouter").is_ok(),
            "expected {url} to be allowed (local-network host)"
        );
    }
}

/// Companion: public IPs and DNS names MUST still be rejected so
/// we don't silently leak api_keys over the internet.
#[test]
fn http_with_api_key_still_rejected_for_remote_hosts() {
    for host in [
        "8.8.8.8",
        "172.32.0.1",  // just outside 172.16/12
        "192.169.0.1", // just outside 192.168/16
        "100.128.0.1", // just outside 100.64/10
        "api.openai.com",
        "openrouter.ai",
    ] {
        let url = format!("http://{host}/v1");
        assert!(
            reject_http_with_api_key(&Some(url.clone()), "key", "openrouter").is_err(),
            "expected {url} to be rejected (remote host)"
        );
    }
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
    assert_eq!(settings.agent.api_key.expose(), "sk-test");
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
    // With no config file, the agent model is empty — `validate_agent_model`
    // (run later by `load_settings`) will reject that so the user can't
    // silently boot on a hardcoded default.
    assert_eq!(settings.agent.model, "");
    assert_eq!(settings.agent.max_iterations, 80);
    assert!(!settings.skills.is_empty());
}

#[test]
fn validate_agent_model_rejects_empty_model() {
    let settings = Settings::default();
    assert!(settings.agent.model.is_empty());
    let err = validate_agent_model(&settings).unwrap_err();
    assert!(format!("{err}").contains("no model configured"));
}

#[test]
fn provider_without_models_is_rejected() {
    let json = r#"{
        "providers": {
            "broken": { "type": "anthropic", "api_key": "sk-x" }
        }
    }"#;
    let root: JsonRoot = serde_json::from_str(json).unwrap();
    let secrets = SecretRegistry::default();
    let settings = build_settings(Some(root), &secrets);
    // Provider was skipped because `models` was empty.
    assert!(!settings.providers.contains_key("broken"));
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
                "models": ["claude-sonnet-4-20250514"],
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
    assert_eq!(settings.agent.api_key.expose(), "from_env");
    assert_eq!(settings.providers["test"].api_key.expose(), "from_env");
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
    assert_eq!(settings.agent.api_key.expose(), "sk-ant");

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
            "claude": {
                "type": "anthropic",
                "models": ["claude-sonnet-4-20250514"],
                "api_key": "sk-test"
            }
        },
        "agent": { "provider": "nonexistent" }
    }"#;
    let root: JsonRoot = serde_json::from_str(json).unwrap();
    let secrets = SecretRegistry::default();
    let settings = build_settings(Some(root), &secrets);

    // Provider name didn't resolve, so nothing overrode the AgentSettings
    // default.  With the registry default removed, the model is blank —
    // `validate_agent_model` (called by `load_settings`) would reject this
    // config so the user has to be explicit.
    assert_eq!(
        settings.agent.provider,
        crate::config::LlmProvider::Anthropic
    );
    assert_eq!(settings.agent.model, "");
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
    assert_eq!(settings.agent.api_key.expose(), "sk-legit-key");
    assert_eq!(
        settings.providers["claude"].api_key.expose(),
        "sk-legit-key"
    );

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
    assert_eq!(settings.agent.api_key.expose(), "sk-explicit-for-proxy");
    assert_eq!(
        settings.agent.base_url.as_deref(),
        Some("https://my-proxy.example.com/v1")
    );
}

#[test]
fn http_with_api_key_remote_rejected() {
    // A provider that would send an api_key over plain HTTP to a
    // non-localhost host is a configuration error, not a warning.
    let json = r#"{
        "providers": {
            "sniffable": {
                "type": "openai",
                "models": ["gpt-4o"],
                "api_key": "sk-should-not-leak",
                "base_url": "http://proxy.example.com/v1"
            }
        },
        "agent": { "provider": "sniffable" }
    }"#;
    let root: JsonRoot = serde_json::from_str(json).unwrap();
    let secrets = SecretRegistry::default();
    let mut settings = build_settings(Some(root), &secrets);
    let err = resolve_api_keys(&mut settings, &secrets).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("plain HTTP"), "unexpected error: {msg}");
    assert!(
        !msg.contains("sk-should-not-leak"),
        "error message leaked the api_key: {msg}"
    );
}

#[test]
fn http_with_api_key_localhost_allowed() {
    // Plain HTTP to localhost is fine — common for Ollama / vLLM.
    let json = r#"{
        "providers": {
            "ollama": {
                "type": "openai",
                "models": ["llama3"],
                "api_key": "sk-local",
                "base_url": "http://127.0.0.1:11434/v1"
            }
        },
        "agent": { "provider": "ollama" }
    }"#;
    let root: JsonRoot = serde_json::from_str(json).unwrap();
    let secrets = SecretRegistry::default();
    let mut settings = build_settings(Some(root), &secrets);
    assert!(resolve_api_keys(&mut settings, &secrets).is_ok());
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
fn validate_rejects_subagent_with_unknown_provider() {
    let mut settings = Settings::default();
    // Note: no providers configured — "gpt" should be unknown.
    settings
        .skills
        .push(SkillConfig::Subagent(SubagentSkillConfig {
            agents: vec![SubagentAgentConfig {
                name: "bad".into(),
                description: "d".into(),
                system_prompt: "p".into(),
                provider: "gpt".into(),
                model: None,
                max_iterations: None,
                max_tokens: None,
                tools: None,
                injects_protocol: None,
            }],
        }));

    let err = validate_subagent_configs(&settings).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("bad"));
    assert!(msg.contains("gpt"));
}

#[test]
fn validate_accepts_default_provider() {
    let mut settings = Settings::default();
    settings
        .skills
        .push(SkillConfig::Subagent(SubagentSkillConfig {
            agents: vec![SubagentAgentConfig {
                name: "good".into(),
                description: "d".into(),
                system_prompt: "p".into(),
                provider: "default".into(),
                model: None,
                max_iterations: None,
                max_tokens: None,
                tools: None,
                injects_protocol: None,
            }],
        }));

    validate_subagent_configs(&settings).expect("default provider should validate");
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

// H3: dyson.json may contain literal API keys, so a world/group readable
// file on disk is a credential-leak hazard. read_config_file must tighten
// perms to 0o600 best-effort and warn when found loose.
#[cfg(unix)]
#[test]
fn read_config_file_tightens_loose_perms() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!(
        "dyson-cfg-perm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dyson.json");
    std::fs::write(&path, "{}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let _ = read_config_file(&path).expect("read_config_file should succeed");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o077,
        0,
        "config file must not be group/world readable after load (got {mode:o})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
