use super::auth::CONFIGURE_HASH_FILENAME;
use super::config::*;
use super::state_files::clean_relative_path;
use super::*;
use std::path::{Path, PathBuf};

#[test]
fn configure_body_accepts_full_swarm_wire_contract() {
    let body: ConfigureBody = serde_json::from_value(serde_json::json!({
        "name": "axelrod",
        "task": "research",
        "models": ["deepseek/deepseek-v4-pro"],
        "instance_id": "i-1",
        "proxy_token": "pt_12345678901234567890123456789012",
        "proxy_base": "https://swarm.test/llm/openrouter",
        "image_provider_name": "image",
        "image_provider_block": {
            "type": "openrouter",
            "api_key": "pt_test",
            "models": ["google/gemini-image"]
        },
        "image_generation_provider": "image",
        "image_generation_model": "google/gemini-image",
        "reset_skills": true,
        "tools": ["read_file"],
        "mcp_servers": {
            "massive": { "url": "https://swarm.test/mcp/i-1/massive" }
        },
        "ingest_url": "https://swarm.test/v1/internal/ingest",
        "ingest_token": "it_12345678901234567890123456789012",
        "state_sync_url": "https://swarm.test/v1/internal/state/file",
        "state_sync_token": "st_12345678901234567890123456789012"
    }))
    .unwrap();

    assert_eq!(body.name.as_deref(), Some("axelrod"));
    assert_eq!(body.models, vec!["deepseek/deepseek-v4-pro"]);
    assert_eq!(body.tools.as_deref(), Some(&["read_file".to_string()][..]));
    assert!(body.reset_skills);
    assert!(body.mcp_servers.as_ref().unwrap().contains_key("massive"));
    // Token comes in as a typed StateSyncToken, parsed by serde via
    // its validating Deserialize impl.  Reach through `.as_str()`
    // for the wire-shape check.
    assert_eq!(
        body.state_sync_token
            .as_ref()
            .map(crate::tokens::StateSyncToken::as_str),
        Some("st_12345678901234567890123456789012")
    );
}

#[test]
fn runtime_patch_requires_complete_pairs_and_supports_clear() {
    assert_eq!(
        runtime_patch(Some("https://swarm.test/ingest"), Some("it_123")),
        Some(RuntimePatch::Set {
            url: "https://swarm.test/ingest",
            token: "it_123"
        })
    );
    assert_eq!(runtime_patch(Some(""), Some("")), Some(RuntimePatch::Clear));
    assert_eq!(runtime_patch(Some("https://swarm.test/ingest"), None), None);
    assert_eq!(runtime_patch(None, Some("it_123")), None);
}

#[test]
fn eager_config_reload_preserves_cli_only_sandbox_flag() {
    let snapshot = Settings {
        sandbox_bypass: Some(crate::sandbox::SandboxBypassGuard::for_test()),
        ..Default::default()
    };
    let mut reloaded = Settings::default();
    assert!(reloaded.sandbox_bypass.is_none());

    preserve_runtime_only_settings(&mut reloaded, &snapshot);

    assert!(reloaded.sandbox_bypass.is_some());
}

#[test]
fn build_identity_md_skips_empty_sections() {
    let s = build_identity_md(Some("Bob"), Some("u1"), None);
    assert!(s.contains("Name: Bob"));
    assert!(s.contains("Swarm instance id: u1"));
    assert!(!s.contains("## Mission"));
}

#[test]
fn build_identity_md_full() {
    let s = build_identity_md(Some("Bob"), Some("u1"), Some("Watch PRs."));
    assert!(s.contains("## Mission\n\nWatch PRs."));
}

#[test]
fn build_identity_md_keeps_full_identity_doc_exact() {
    let full = "# IDENTITY.md — Who Am I?\n\n- **Name:** axelrod";
    let s = build_identity_md(Some("Bob"), Some("u1"), Some(full));
    assert_eq!(s, full);
    assert!(!s.contains("Swarm instance id:"));
    assert!(!s.contains("## Mission"));
}

#[test]
fn existing_full_identity_doc_is_preserved_as_prior_mission() {
    let existing = "# IDENTITY.md — Who Am I?\n\n- **Name:** axelrod";
    let prior = extract_section(existing, "Mission")
        .or_else(|| looks_like_full_identity_doc(existing).then(|| existing.to_owned()));
    assert_eq!(prior.as_deref(), Some(existing));
}

#[test]
fn extract_field_picks_first_match() {
    let b = "Name: Alice\nSwarm instance id: u9\n";
    assert_eq!(extract_field(b, "Name"), Some("Alice".into()));
    assert_eq!(extract_field(b, "Swarm instance id"), Some("u9".into()));
    assert_eq!(extract_field(b, "Missing"), None);
}

#[test]
fn extract_section_keeps_only_named_block() {
    let b = "# Identity\n\nName: A\n\n## Mission\n\nDo the thing.\n\n## Other\n\nelse";
    assert_eq!(extract_section(b, "Mission"), Some("Do the thing.".into()));
    assert_eq!(extract_section(b, "Other"), Some("else".into()));
    assert_eq!(extract_section(b, "Nope"), None);
}

#[test]
fn patch_models_round_trip() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": {
            "provider": "openrouter",
            "model": "warmup-placeholder"
        },
        "providers": {
            "openrouter": {
                "type": "openai",
                "api_key": "warmup-placeholder",
                "models": ["warmup-placeholder"]
            }
        }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    // `base_url` must NOT carry `/v1` — `OpenAiCompatClient` appends
    // `/v1/chat/completions` itself when building the request URL.  A
    // base ending in `/v1` doubles up to `/openrouter/v1/v1/...`,
    // which routes to OR's marketing site and surfaces as a generic
    // "upstream HTTP error".
    patch_provider_in_config(
        &path,
        Some(&["anthropic/claude-sonnet-4-5".into(), "openai/gpt-5".into()]),
        Some("dy-real-token"),
        Some("https://dyson.example/llm/openrouter"),
    )
    .unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after["agent"]["model"], "anthropic/claude-sonnet-4-5",
        "reconfigure must clear the boot-time agent.model override"
    );
    let prov = &after["providers"]["openrouter"];
    assert_eq!(prov["api_key"], "dy-real-token");
    assert_eq!(prov["base_url"], "https://dyson.example/llm/openrouter");
    let models = prov["models"].as_array().unwrap();
    assert_eq!(models[0], "anthropic/claude-sonnet-4-5");
    assert_eq!(models[1], "openai/gpt-5");
}

#[test]
fn patch_image_generation_inserts_provider_and_agent_fields() {
    // Existing config has only the chat provider — the swarm-side
    // rewire sweep arrives with a brand-new image provider block
    // and the agent fields pointing at it.  Both must land in one
    // atomic write so the HotReloader doesn't see a half-state.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": { "provider": "openrouter" },
        "providers": {
            "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] }
        }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    let block = serde_json::json!({
        "type": "openrouter",
        "base_url": "https://swarm/llm/openrouter",
        "api_key": "tok",
        "models": ["google/gemini-3-pro-image-preview"]
    });
    patch_image_generation_in_config(
        &path,
        Some(("openrouter-image", &block)),
        Some("openrouter-image"),
        Some("google/gemini-3-pro-image-preview"),
    )
    .unwrap();

    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let img = &after["providers"]["openrouter-image"];
    assert_eq!(img["type"], "openrouter");
    assert_eq!(img["base_url"], "https://swarm/llm/openrouter");
    assert_eq!(img["models"][0], "google/gemini-3-pro-image-preview");
    assert_eq!(
        after["agent"]["image_generation_provider"],
        "openrouter-image"
    );
    assert_eq!(
        after["agent"]["image_generation_model"],
        "google/gemini-3-pro-image-preview"
    );
    // Chat side is untouched — a regression here would silently
    // break the chat path on every running instance the sweep
    // visits.
    assert_eq!(after["agent"]["provider"], "openrouter");
    assert_eq!(after["providers"]["openrouter"]["api_key"], "x");
}

#[test]
fn named_swarm_provider_patch_preserves_active_subscription_provider() {
    let mut doc = serde_json::json!({
        "agent": {
            "provider": "chatgpt-subscription",
            "model": "gpt-5.6-sol"
        },
        "providers": {
            "openrouter": {
                "type": "openai",
                "api_key": "old",
                "base_url": "https://old.example",
                "models": ["old/model"]
            },
            "chatgpt-subscription": {
                "type": "codex",
                "models": ["gpt-5.6-sol"]
            }
        }
    });
    let models = vec!["anthropic/claude-sonnet-4-5".to_owned()];

    patch_provider_doc(
        &mut doc,
        None,
        Some(&models),
        Some("pt_new"),
        Some("https://swarm.example/llm/openrouter"),
    )
    .unwrap();

    assert_eq!(doc["agent"]["provider"], "chatgpt-subscription");
    assert_eq!(doc["agent"]["model"], "gpt-5.6-sol");
    assert_eq!(doc["providers"]["openrouter"]["api_key"], "pt_new");
    assert_eq!(
        doc["providers"]["openrouter"]["models"],
        serde_json::json!(["anthropic/claude-sonnet-4-5"])
    );
    assert!(doc["providers"]["chatgpt-subscription"]["base_url"].is_null());
}

#[test]
fn durable_selection_replaces_openrouter_startup_default() {
    let mut doc = serde_json::json!({
        "agent": {
            "provider": "openrouter",
            "model": "~moonshotai/kimi-latest"
        },
        "providers": {
            "openrouter": {
                "type": "openai",
                "models": ["~moonshotai/kimi-latest"]
            },
            "chatgpt-subscription": {
                "type": "codex",
                "models": ["gpt-5.6-terra", "gpt-5.6-sol"]
            }
        }
    });

    let changed =
        patch_active_selection_doc(&mut doc, "chatgpt-subscription", "gpt-5.6-sol").unwrap();

    assert!(changed);
    assert_eq!(doc["agent"]["provider"], "chatgpt-subscription");
    assert_eq!(doc["agent"]["model"], "gpt-5.6-sol");
    assert_eq!(
        doc["providers"]["chatgpt-subscription"]["models"][0],
        "gpt-5.6-sol"
    );
    assert_eq!(
        doc["providers"]["openrouter"]["models"][0],
        "~moonshotai/kimi-latest"
    );
}

#[test]
fn runtime_configure_refreshes_subscription_catalogues_after_rotation() {
    let mut doc = serde_json::json!({
        "agent": {
            "provider": "chatgpt-subscription",
            "model": "gpt-5.6-sol"
        },
        "providers": {
            "openrouter": {
                "type": "openai",
                "models": ["swarm/model"]
            },
            "chatgpt-subscription": {
                "type": "codex",
                "models": ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]
            },
            "claude-subscription": {
                "type": "claude-code",
                "models": ["claude-opus-4-6", "claude-sonnet-4-6"]
            }
        }
    });

    assert!(patch_subscription_models_doc(&mut doc).unwrap());
    assert_eq!(
        doc["providers"]["chatgpt-subscription"]["models"],
        serde_json::json!(crate::subscription_models::CHATGPT)
    );
    assert_eq!(
        doc["providers"]["claude-subscription"]["models"],
        serde_json::json!(crate::subscription_models::CLAUDE)
    );
    assert_eq!(doc["agent"]["provider"], "chatgpt-subscription");
    assert_eq!(doc["agent"]["model"], "gpt-5.6-sol");
    assert!(!patch_subscription_models_doc(&mut doc).unwrap());

    doc["agent"]["provider"] = Value::String("claude-subscription".to_owned());
    doc["agent"]["model"] = Value::String("claude-opus-4-6".to_owned());
    assert!(patch_subscription_models_doc(&mut doc).unwrap());
    assert_eq!(doc["agent"]["model"], "claude-fable-5");
    assert!(!patch_subscription_models_doc(&mut doc).unwrap());
}

#[test]
fn patch_image_generation_partial_update_only_touches_provided_fields() {
    // Operator-side: somebody bumps the image model id (e.g. a
    // newer preview) without changing the provider entry.  Only
    // `agent.image_generation_model` should change; the provider
    // block and provider-name field stay as they were.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": {
            "provider": "openrouter",
            "image_generation_provider": "openrouter-image",
            "image_generation_model": "google/old-model"
        },
        "providers": {
            "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] },
            "openrouter-image": { "type": "openrouter", "models": ["google/old-model"] }
        }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    patch_image_generation_in_config(&path, None, None, Some("google/new-model")).unwrap();
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["agent"]["image_generation_model"], "google/new-model");
    assert_eq!(
        after["agent"]["image_generation_provider"],
        "openrouter-image"
    );
    // Provider block models[] left alone — the model id only flows
    // through the agent.image_generation_model override.
    assert_eq!(
        after["providers"]["openrouter-image"]["models"][0],
        "google/old-model"
    );
}

#[test]
fn clear_skills_drops_the_block_so_loader_registers_all_builtins() {
    // Regression for "the agent has no tools".  Older `dyson swarm`
    // boots wrote `skills.builtin.tools = []`, which the loader
    // parses as "register zero builtin tools".  The configure-time
    // skills reset must remove the key entirely so the loader's
    // no-skills-block branch fires on next reload.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": { "provider": "openrouter" },
        "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } },
        "skills": { "builtin": { "tools": [] } }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    assert!(
        clear_skills_in_config(&path).unwrap(),
        "first call must report a change"
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(after.get("skills").is_none(), "skills key must be removed");
    // Other top-level keys unchanged — clearing skills must not
    // perturb providers / agent.
    assert_eq!(after["agent"]["provider"], "openrouter");
    assert_eq!(after["providers"]["openrouter"]["api_key"], "x");

    // Idempotent second call: no skills key means nothing to remove.
    assert!(
        !clear_skills_in_config(&path).unwrap(),
        "second call must report no-op when skills already absent"
    );
}

#[test]
fn set_skills_tools_writes_explicit_allowlist_and_is_idempotent() {
    // Editing an instance's tool selection in the orchestrator UI
    // must rewrite `skills.builtin.tools` to the chosen subset on
    // the running dyson; otherwise the live agent keeps registering
    // the boot-time set.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": { "provider": "openrouter" },
        "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    let tools = vec!["bash".to_string(), "read_file".to_string()];
    assert!(
        set_skills_tools_in_config(&path, &tools).unwrap(),
        "first write must report a change"
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after["skills"]["builtin"]["tools"],
        serde_json::json!(["bash", "read_file"])
    );
    // Sibling keys preserved.
    assert_eq!(after["agent"]["provider"], "openrouter");
    assert_eq!(after["providers"]["openrouter"]["api_key"], "x");

    // Same allowlist a second time is a no-op.
    assert!(
        !set_skills_tools_in_config(&path, &tools).unwrap(),
        "no-change call must report no-op"
    );

    // Empty list lands as `tools: []` so the loader registers zero builtins.
    assert!(
        set_skills_tools_in_config(&path, &[]).unwrap(),
        "shrink to empty must report a change"
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["skills"]["builtin"]["tools"], serde_json::json!([]));
}

#[test]
fn set_skills_tools_filters_subagents_by_same_allowlist() {
    // The orchestrator's tool-picker collapses builtins and
    // subagents into one checklist; unchecking a subagent in the
    // SPA must drop it from the running dyson too.  Otherwise the
    // agent introspects its loaded subagents and reports them as
    // available even though the operator disabled them — which is
    // the bug this rule fixes.
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": { "provider": "openrouter" },
        "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } },
        "skills": {
            "builtin": { "tools": ["read_file", "write_file"] },
            "subagents": [
                { "name": "planner",     "description": "p", "system_prompt": "sp" },
                { "name": "researcher",  "description": "r", "system_prompt": "sr" },
                { "name": "coder",       "description": "c", "system_prompt": "sc" }
            ]
        }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    // Allowlist keeps two builtins + one of the three subagents.
    let allow = vec![
        "read_file".to_string(),
        "write_file".to_string(),
        "planner".to_string(),
    ];
    assert!(
        set_skills_tools_in_config(&path, &allow).unwrap(),
        "filtering must report a change"
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

    // Builtin allowlist round-trips verbatim — including the two
    // subagent-shaped names.  parse_skills' loader-side filter
    // ignores names that don't match a real builtin, so the only
    // tools that actually register are the genuine builtins.
    assert_eq!(
        after["skills"]["builtin"]["tools"],
        serde_json::json!(["read_file", "write_file", "planner"])
    );

    // Subagents: only "planner" survives; "researcher" and "coder"
    // are gone because they weren't in the allowlist.  The other
    // fields on the kept entry round-trip verbatim.
    let subagents = after["skills"]["subagents"].as_array().unwrap();
    assert_eq!(subagents.len(), 1, "only planner should survive");
    assert_eq!(subagents[0]["name"], "planner");
    assert_eq!(subagents[0]["description"], "p");
    assert_eq!(subagents[0]["system_prompt"], "sp");

    // Same call a second time is a no-op (idempotent).
    assert!(
        !set_skills_tools_in_config(&path, &allow).unwrap(),
        "no-change call must report no-op"
    );

    // Allowlist that excludes every subagent drops the subagents
    // key entirely — keeps the loader's contract that an empty
    // array doesn't get a Subagent skill config pushed.
    let no_subagents = vec!["read_file".to_string()];
    assert!(
        set_skills_tools_in_config(&path, &no_subagents).unwrap(),
        "narrowing the allowlist must report a change"
    );
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        after["skills"].get("subagents").is_none(),
        "subagents key must be absent when the allowlist excludes every entry, got {:?}",
        after["skills"].get("subagents")
    );
    assert_eq!(
        after["skills"]["builtin"]["tools"],
        serde_json::json!(["read_file"])
    );
}

#[test]
fn patch_mcp_servers_replaces_block_and_is_idempotent() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dyson.json");
    let initial = serde_json::json!({
        "agent": { "provider": "openrouter" },
        "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } }
    });
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice())
        .unwrap();
    drop(f);

    // Insert a server.
    let mut servers = serde_json::Map::new();
    servers.insert(
        "linear".into(),
        serde_json::json!({
            "url": "https://swarm.example/mcp/i-1/linear",
            "headers": { "Authorization": "Bearer tok" }
        }),
    );
    assert!(patch_mcp_servers_in_config(&path, &servers).unwrap());
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after["mcp_servers"]["linear"]["url"],
        "https://swarm.example/mcp/i-1/linear"
    );
    // Sibling keys untouched.
    assert_eq!(after["agent"]["provider"], "openrouter");

    // Idempotent: the same map yields no rewrite.
    assert!(!patch_mcp_servers_in_config(&path, &servers).unwrap());

    // Empty map clears the block.
    let empty = serde_json::Map::new();
    assert!(patch_mcp_servers_in_config(&path, &empty).unwrap());
    let after2: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(after2.get("mcp_servers").is_none());
}

#[test]
fn clean_relative_path_rejects_escape_paths() {
    assert_eq!(
        clean_relative_path("c-1/transcript.json").unwrap(),
        PathBuf::from("c-1").join("transcript.json")
    );
    assert!(clean_relative_path("../secret").is_err());
    assert!(clean_relative_path("/etc/passwd").is_err());
    assert!(clean_relative_path("c-1/../../secret").is_err());
    assert!(clean_relative_path("c-1\\transcript.json").is_err());
    assert!(clean_relative_path("").is_err());
}

// H4: closes the TOFU race window when swarm pre-seeds the configure
// secret at provisioning time via a writable-layer file. The first
// POST to /api/admin/configure must verify against the pre-seeded
// hash, not mint a fresh one — otherwise an attacker who reaches the
// controller before swarm can adopt admin rights.
#[test]
fn preseed_configure_hash_consumes_file_and_writes_hash() {
    let dir = tempfile::tempdir().unwrap();
    let preseed = dir.path().join("configure.preseed");
    let hash_path = dir.path().join(CONFIGURE_HASH_FILENAME);
    std::fs::write(&preseed, "swarm-supplied-secret").unwrap();
    assert!(!hash_path.exists());

    let consumed = preseed_configure_hash(dir.path()).expect("preseed consumption");
    assert!(consumed, "consumed flag must be true when preseed present");
    assert!(!preseed.exists(), "preseed file must be deleted after use");

    let stored = std::fs::read_to_string(&hash_path).expect("hash file written");
    assert!(
        stored.starts_with("$argon2id$"),
        "hash must be PHC argon2id"
    );

    use argon2::Argon2;
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let parsed = PasswordHash::new(stored.trim()).expect("parse PHC");
    assert!(
        Argon2::default()
            .verify_password(b"swarm-supplied-secret", &parsed)
            .is_ok(),
        "stored hash must verify against the preseeded secret"
    );
}

#[test]
fn preseed_configure_hash_noop_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let consumed = preseed_configure_hash(dir.path()).expect("noop call");
    assert!(!consumed);
    assert!(!dir.path().join(CONFIGURE_HASH_FILENAME).exists());
}

#[test]
fn preseed_configure_hash_refuses_to_clobber_existing_hash() {
    let dir = tempfile::tempdir().unwrap();
    let preseed = dir.path().join("configure.preseed");
    let hash_path = dir.path().join(CONFIGURE_HASH_FILENAME);
    std::fs::write(&preseed, "new-secret").unwrap();
    std::fs::write(&hash_path, "$argon2id$v=19$existing").unwrap();

    let consumed = preseed_configure_hash(dir.path()).expect("idempotent call");
    assert!(!consumed, "must not clobber an already-minted hash");
    // Preseed is still consumed (removed) to drop a stale plaintext.
    assert!(!preseed.exists());
    // Existing hash is preserved.
    assert_eq!(
        std::fs::read_to_string(&hash_path).unwrap(),
        "$argon2id$v=19$existing"
    );
}
// These adapters exercise the production transaction, including its atomic write.
fn apply_config_patch(
    path: &Path,
    patch: ConfigureConfigPatch<'_>,
) -> Result<AppliedConfigPatch, ConfigureConfigPatchError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(patch_config_once(path, patch))
}

fn patch_provider_in_config(
    path: &Path,
    models: Option<&[String]>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<(), ConfigureConfigPatchError> {
    apply_config_patch(
        path,
        ConfigureConfigPatch {
            models,
            api_key,
            base_url,
            ..Default::default()
        },
    )
    .map(|_| ())
}

fn patch_image_generation_in_config(
    path: &Path,
    provider_block: Option<(&str, &Value)>,
    image_provider: Option<&str>,
    image_model: Option<&str>,
) -> Result<(), ConfigureConfigPatchError> {
    apply_config_patch(
        path,
        ConfigureConfigPatch {
            image_provider_block: provider_block,
            image_provider,
            image_model,
            ..Default::default()
        },
    )
    .map(|_| ())
}

fn clear_skills_in_config(path: &Path) -> Result<bool, ConfigureConfigPatchError> {
    apply_config_patch(
        path,
        ConfigureConfigPatch {
            reset_skills: true,
            ..Default::default()
        },
    )
    .map(|applied| applied.skills_changed)
}

fn set_skills_tools_in_config(
    path: &Path,
    tools: &[String],
) -> Result<bool, ConfigureConfigPatchError> {
    apply_config_patch(
        path,
        ConfigureConfigPatch {
            tools: Some(tools),
            ..Default::default()
        },
    )
    .map(|applied| applied.skills_changed)
}

fn patch_mcp_servers_in_config(
    path: &Path,
    servers: &serde_json::Map<String, Value>,
) -> Result<bool, ConfigureConfigPatchError> {
    apply_config_patch(
        path,
        ConfigureConfigPatch {
            mcp_servers: Some(servers),
            ..Default::default()
        },
    )
    .map(|applied| applied.mcp_changed)
}
