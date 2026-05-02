// ===========================================================================
// `dyson swarm` — boot mode for running inside a CubeSandbox MicroVM
// under the dyson-orchestrator (swarm).
//
// Reads the env envelope swarm injects on sandbox creation:
//   - SWARM_BEARER_TOKEN — auth secret the dyson_proxy stamps on every
//     forwarded request
//   - SWARM_PROXY_URL    — base URL of swarm's /llm provider proxy
//   - SWARM_PROXY_TOKEN  — bearer for that proxy
//   - SWARM_MODEL        — model id the agent talks to (e.g.
//                            "anthropic/claude-sonnet-4-5",
//                            "openai/gpt-4o"). No default — empty in
//                            warmup mode, required at instance boot.
//   - SWARM_TASK         — free-text task description (seeded into
//                            workspace/TASK.md)
//   - SWARM_NAME         — human-readable label
//   - SWARM_INSTANCE_ID  — swarm-side instance id
//   - SWARM_STATE_SYNC_* — optional parent-swarm state mirror target
//
// Provider shape: swarm's /llm proxy fronts upstream LLM APIs. We
// configure the dyson agent as an `openai`-compatible client pointed at
// `<SWARM_PROXY_URL>/openrouter` — OpenRouter speaks the OpenAI Chat
// Completions protocol, so the same client transport works for any of
// its 200+ models. Switching to a different provider later is a one-
// path-segment change in this file.
//
// Synthesises a minimal dyson.json + a workspace skeleton, then hands off
// to the standard `listen` runtime so the HTTP controller serves the
// dyson_proxy on the standard port. There is no native sandbox inside
// the Cube VM (the VM is the sandbox); we pass `dangerous_no_sandbox`
// so the agent loop accepts that posture.
// ===========================================================================

use std::path::PathBuf;

use serde_json::json;

use dyson::auth::HashedBearerAuth;
use dyson::error::{DysonError, Result};

const DEFAULT_BIND: &str = "0.0.0.0:80";
const DEFAULT_DYSON_HOME: &str = "/var/lib/dyson";

/// Provider name in `providers` that the `image_generate` tool reads
/// from.  Separate from the chat provider so the chat path keeps its
/// (working) `type: openai` shape and the image factory dispatches on
/// `LlmProvider::OpenRouter` for the modalities-aware code path.
const IMAGE_PROVIDER_NAME: &str = "openrouter-image";

/// Default image generation model on OpenRouter.  Picked because
/// Google's Gemini 3 image preview is the highest-quality general
/// image model available through the OpenRouter proxy today.
const DEFAULT_IMAGE_MODEL: &str = "google/gemini-3-pro-image-preview";

pub async fn run() -> Result<()> {
    // SWARM_BEARER_TOKEN is the per-instance auth secret swarm injects on
    // create. It's NOT set during template build — Cube boots the rootfs
    // once to probe /healthz and snapshot; only post-snapshot restores
    // (instance creates) carry the env envelope. So treat the unset case
    // as "warmup" mode: bind with no inbound auth, serve /healthz, get
    // snapshotted. When swarm later restarts us with the env set, the
    // bearer takes effect.
    let bearer = std::env::var("SWARM_BEARER_TOKEN").unwrap_or_default();
    let warmup = bearer.is_empty();
    if warmup {
        tracing::warn!(
            "SWARM_BEARER_TOKEN unset — running in template-warmup mode with \
             dangerous_no_auth on the HTTP controller. Expected during cube \
             template build; swarm injects the bearer on instance create."
        );
    }

    let bind = std::env::var("DYSON_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
    let home = std::env::var("DYSON_HOME").unwrap_or_else(|_| DEFAULT_DYSON_HOME.into());
    let proxy_url = std::env::var("SWARM_PROXY_URL").unwrap_or_default();
    let proxy_token = std::env::var("SWARM_PROXY_TOKEN").unwrap_or_default();
    let task = std::env::var("SWARM_TASK").unwrap_or_default();
    let name = std::env::var("SWARM_NAME").unwrap_or_default();
    let instance_id = std::env::var("SWARM_INSTANCE_ID").unwrap_or_default();
    let model = std::env::var("SWARM_MODEL").unwrap_or_default();
    let state_sync = dyson::swarm_state_sync::config_from_env();
    // Optional builtin-tool allowlist.  Swarm only stamps this on the
    // env envelope when the operator picked a strict subset (or asked
    // for zero tools); when unset, dyson registers every builtin.
    // Empty CSV ("") means "register zero tools".
    let tools_csv = std::env::var("SWARM_TOOLS").ok();

    let home_path = PathBuf::from(&home);
    std::fs::create_dir_all(&home_path)
        .map_err(|e| DysonError::Config(format!("create dyson home {home}: {e}")))?;
    let workspace = home_path.join("workspace");
    std::fs::create_dir_all(&workspace)
        .map_err(|e| DysonError::Config(format!("create workspace {workspace:?}: {e}")))?;
    let chats = home_path.join("chats");
    std::fs::create_dir_all(&chats)
        .map_err(|e| DysonError::Config(format!("create chats dir {chats:?}: {e}")))?;

    // IDENTITY.md is what `Workspace::system_prompt` injects under the
    // "## IDENTITY" section of the agent's system prompt — so this is
    // the file the model actually reads on every turn.  Bake the task
    // in here too so the agent has its mission alongside its name.
    // (TASK.md isn't read by the workspace prompt builder, so writing
    // it separately would be a dead drop.)
    if !name.is_empty() || !instance_id.is_empty() || !task.is_empty() {
        let identity = workspace.join("IDENTITY.md");
        if !identity.exists() {
            let mut body = String::from("# Identity\n\n");
            if !name.is_empty() {
                body.push_str(&format!("Name: {name}\n"));
            }
            if !instance_id.is_empty() {
                body.push_str(&format!("Swarm instance id: {instance_id}\n"));
            }
            if !task.is_empty() {
                body.push_str(&format!("\n## Mission\n\n{task}\n"));
            }
            let _ = std::fs::write(&identity, body);
        }
    }

    let auth_block = if warmup {
        json!({ "type": "dangerous_no_auth" })
    } else {
        let bearer_hash = HashedBearerAuth::hash(&bearer)?;
        json!({ "type": "bearer", "hash": bearer_hash })
    };

    // Provider config — swarm's /llm proxy fronts the upstream LLM APIs.
    // For the smoke test the agent is never invoked; the provider just
    // needs to parse cleanly so `listen` can come up. Dyson's loader
    // refuses base_url with an empty api_key (defends against env-var
    // fallback to a non-default endpoint) — supply a placeholder when
    // swarm hasn't set the proxy token (warmup).
    //
    // Dyson's loader also refuses to boot with no model set (validate_agent_model).
    // In warmup the agent never runs, so a placeholder model is fine; at
    // instance boot the operator must supply SWARM_MODEL via the create
    // request's env (swarm refuses the create otherwise — see the
    // orchestrator's instance.rs).
    let api_key = if proxy_token.is_empty() {
        "warmup-placeholder".to_string()
    } else {
        proxy_token
    };
    let model_id = if model.is_empty() {
        "warmup-placeholder".to_string()
    } else {
        model
    };
    let workspace_str = workspace.to_string_lossy().into_owned();
    let chats_str = chats.to_string_lossy().into_owned();
    let tools = tools_csv.as_deref().map(|csv| {
        csv.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    });
    let cfg = build_swarm_config(SwarmConfigInputs {
        bind: &bind,
        proxy_url: &proxy_url,
        api_key: &api_key,
        model_id: &model_id,
        workspace_str: &workspace_str,
        chats_str: &chats_str,
        auth_block,
        tools: tools.as_deref(),
    });

    let cfg_path = home_path.join("dyson.json");
    let cfg_bytes = serde_json::to_vec_pretty(&cfg)
        .map_err(|e| DysonError::Config(format!("serialize dyson.json: {e}")))?;
    std::fs::write(&cfg_path, cfg_bytes)
        .map_err(|e| DysonError::Config(format!("write {cfg_path:?}: {e}")))?;

    dyson::swarm_state_sync::spawn_worker(workspace.clone(), chats.clone(), state_sync);

    tracing::info!(
        bind = %bind,
        instance = %instance_id,
        name = %name,
        task_set = !task.is_empty(),
        "dyson swarm — starting HTTP controller"
    );

    super::listen::run(Some(cfg_path), true, None, None, None).await
}

/// Inputs threaded into `build_swarm_config`.  Borrowed strings keep
/// the call site free of clones; the function returns a fresh JSON
/// value so the caller owns it for serialisation.
struct SwarmConfigInputs<'a> {
    bind: &'a str,
    /// Empty when `dyson swarm` is in template-warmup mode (no swarm
    /// proxy URL injected yet) — provider blocks fall back to direct
    /// OpenAI / OpenRouter endpoints with the placeholder api_key.
    proxy_url: &'a str,
    api_key: &'a str,
    model_id: &'a str,
    workspace_str: &'a str,
    chats_str: &'a str,
    auth_block: serde_json::Value,
    /// Builtin-tool allowlist parsed from `SWARM_TOOLS`.  `None` ⇒ omit
    /// the `skills` block (loader interprets as "all builtins").
    /// `Some(&[])` ⇒ write an explicit empty `tools: []` so the loader
    /// registers zero builtins.  `Some(&[..])` ⇒ register exactly those.
    tools: Option<&'a [String]>,
}

/// Render the dyson.json body for a swarm-mode boot.  Pure — no
/// filesystem or env access — so it's directly testable.
fn build_swarm_config(inputs: SwarmConfigInputs<'_>) -> serde_json::Value {
    let chat_block = if inputs.proxy_url.is_empty() {
        json!({
            "type": "openai",
            "api_key": inputs.api_key,
            "models": [inputs.model_id]
        })
    } else {
        json!({
            "type": "openai",
            "base_url": swarm_provider_base_url(inputs.proxy_url),
            "api_key": inputs.api_key,
            "models": [inputs.model_id]
        })
    };

    // Image generation provider — a second openrouter-typed entry
    // dedicated to the `image_generate` tool.  It rides the same
    // `<proxy_base>/openrouter` swarm proxy as chat (so we don't need
    // a second API key), but `image_generate.rs` dispatches on
    // `provider_type`: only `LlmProvider::OpenRouter` and `Gemini`
    // are wired for image generation today.  Keeping it as a separate
    // provider entry leaves the chat path's `type: openai` exactly as
    // it was — switching the chat type to `openrouter` would change
    // which `LlmClient` builds (`OpenRouterClient` ignores `base_url`
    // so the swarm proxy hop would silently disappear).
    let image_block = if inputs.proxy_url.is_empty() {
        None
    } else {
        Some(json!({
            "type": "openrouter",
            "base_url": swarm_provider_base_url(inputs.proxy_url),
            "api_key": inputs.api_key,
            "models": [DEFAULT_IMAGE_MODEL]
        }))
    };

    let mut providers = json!({ "openrouter": chat_block });
    let mut agent = json!({ "provider": "openrouter" });
    if let Some(block) = image_block {
        providers[IMAGE_PROVIDER_NAME] = block;
        agent["image_generation_provider"] = json!(IMAGE_PROVIDER_NAME);
        agent["image_generation_model"] = json!(DEFAULT_IMAGE_MODEL);
    }

    // `skills` is omitted unless the operator supplied an allowlist via
    // `SWARM_TOOLS`.  The dyson loader treats an absent `skills` block
    // (or an absent `skills.builtin`) as "wire every builtin tool"; an
    // EXPLICIT `"builtin": { "tools": [...] }` is parsed as that exact
    // set, and `"tools": []` as zero builtins.  Earlier versions of
    // this writer emitted the explicit empty array unconditionally and
    // shipped every dyson swarm instance toolless — bash, read_file,
    // image_generate, the lot, all silently absent.  Omitting the key
    // is the correct way to say "give me the defaults".
    let mut cfg = json!({
        "config_version": 2,
        "providers": providers,
        "agent": agent,
        "controllers": [
            {
                "type": "http",
                "bind": inputs.bind,
                "auth": inputs.auth_block,
                "dangerous_no_tls": true
            }
        ],
        "workspace": { "connection_string": inputs.workspace_str },
        "chat_history": {
            "backend": "disk",
            "connection_string": inputs.chats_str
        }
    });
    if let Some(tools) = inputs.tools {
        cfg["skills"] = json!({ "builtin": { "tools": tools } });
    }
    cfg
}

/// Build the `providers.openrouter.base_url` value that lands in dyson.json.
///
/// `OpenAiCompatClient` appends `/v1/chat/completions` to whatever
/// `base_url` it sees, so this helper deliberately stops at `/openrouter`.
/// A base ending in `/v1` doubles up to `/openrouter/v1/v1/...`, routes to
/// OR's marketing site, and dyson surfaces the resulting non-200 as a
/// generic "upstream HTTP error" — the bug pinned by the regression test
/// `swarm_provider_base_url_has_no_trailing_v1`.
fn swarm_provider_base_url(proxy_url: &str) -> String {
    format!("{}/openrouter", proxy_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swarm_provider_base_url_has_no_trailing_v1() {
        // The contract: swarm's /llm proxy URL gets one provider segment
        // appended.  `OpenAiCompatClient::stream` then appends
        // `/v1/chat/completions` itself.  Adding `/v1` here would
        // double it up.
        assert_eq!(
            swarm_provider_base_url("https://dyson.example.com/llm"),
            "https://dyson.example.com/llm/openrouter"
        );
    }

    #[test]
    fn swarm_provider_base_url_strips_trailing_slash() {
        assert_eq!(
            swarm_provider_base_url("https://dyson.example.com/llm/"),
            "https://dyson.example.com/llm/openrouter"
        );
    }

    fn cfg_with_proxy() -> serde_json::Value {
        build_swarm_config(SwarmConfigInputs {
            bind: "0.0.0.0:80",
            proxy_url: "https://dyson.example.com/llm",
            api_key: "swarm-token",
            model_id: "anthropic/claude-sonnet-4-5",
            workspace_str: "/var/lib/dyson/workspace",
            chats_str: "/var/lib/dyson/chats",
            auth_block: json!({ "type": "dangerous_no_auth" }),
            tools: None,
        })
    }

    #[test]
    fn swarm_config_registers_openrouter_image_provider_by_default() {
        // The swarm-mode dyson.json must auto-wire the image_generate
        // tool to OpenRouter through the same /llm proxy as chat — so
        // any deployed instance can produce images out of the box,
        // without operators editing per-instance config.
        let cfg = cfg_with_proxy();
        let img = &cfg["providers"]["openrouter-image"];
        assert_eq!(img["type"], "openrouter");
        assert_eq!(img["base_url"], "https://dyson.example.com/llm/openrouter");
        assert_eq!(img["models"][0], "google/gemini-3-pro-image-preview");

        let agent = &cfg["agent"];
        assert_eq!(agent["provider"], "openrouter");
        assert_eq!(agent["image_generation_provider"], "openrouter-image");
        assert_eq!(
            agent["image_generation_model"],
            "google/gemini-3-pro-image-preview"
        );
    }

    #[test]
    fn swarm_config_chat_provider_unchanged_after_image_wiring() {
        // Belt-and-braces: adding the second provider entry must not
        // perturb the chat shape — `type: openai`, base_url stops at
        // /openrouter, the configured chat model is the only entry in
        // models[].  These bits are exactly what previous regressions
        // pinned (`swarm_provider_base_url_has_no_trailing_v1`,
        // `create_pushes_proxy_base_without_trailing_v1`).
        let cfg = cfg_with_proxy();
        let chat = &cfg["providers"]["openrouter"];
        assert_eq!(chat["type"], "openai");
        assert_eq!(chat["base_url"], "https://dyson.example.com/llm/openrouter");
        assert_eq!(chat["models"], json!(["anthropic/claude-sonnet-4-5"]));
        assert_eq!(chat["api_key"], "swarm-token");
    }

    #[test]
    fn swarm_config_omits_skills_block_to_inherit_all_builtin_tools() {
        // Regression for "the agent has no tools".  An explicit
        // `skills.builtin = { tools: [] }` block is parsed by the
        // dyson loader as "register zero builtin tools" — the agent
        // boots without bash, read_file, image_generate, anything.
        // Omitting the skills key entirely is the only way to inherit
        // the full builtin toolbox on a swarm-managed dyson.
        let cfg = cfg_with_proxy();
        assert!(
            cfg.get("skills").is_none(),
            "swarm config must NOT carry a skills key — that triggers the \
             loader's explicit-empty-array branch and ships a toolless dyson"
        );
    }

    #[test]
    fn swarm_config_pins_chat_history_inside_dyson_home() {
        let cfg = cfg_with_proxy();
        assert_eq!(cfg["chat_history"]["backend"], "disk");
        assert_eq!(
            cfg["chat_history"]["connection_string"],
            "/var/lib/dyson/chats"
        );
    }

    #[test]
    fn swarm_config_in_warmup_mode_has_no_image_provider() {
        // No proxy URL ⇒ template-warmup boot.  Image generation is
        // pointless until the per-instance proxy URL is patched in,
        // and a `type: openrouter` block with a placeholder api_key
        // pointing at api.openrouter.ai would 401 noisily.  Skip it.
        let cfg = build_swarm_config(SwarmConfigInputs {
            bind: "0.0.0.0:80",
            proxy_url: "",
            api_key: "warmup-placeholder",
            model_id: "warmup-placeholder",
            workspace_str: "/var/lib/dyson/workspace",
            chats_str: "/var/lib/dyson/chats",
            auth_block: json!({ "type": "dangerous_no_auth" }),
            tools: None,
        });
        assert!(cfg["providers"]["openrouter-image"].is_null());
        assert!(cfg["agent"]["image_generation_provider"].is_null());
        assert!(cfg["agent"]["image_generation_model"].is_null());
    }

    #[test]
    fn swarm_config_writes_explicit_tool_allowlist_when_supplied() {
        // When swarm passes a `SWARM_TOOLS` allowlist, the swarm-mode
        // config writer must emit `skills.builtin.tools = [..]` so the
        // dyson loader registers exactly that subset.  Previously the
        // writer ignored the env var and always shipped every builtin,
        // which is what operators were seeing in production.
        let tools = vec!["bash".to_string(), "read_file".to_string()];
        let cfg = build_swarm_config(SwarmConfigInputs {
            bind: "0.0.0.0:80",
            proxy_url: "https://dyson.example.com/llm",
            api_key: "swarm-token",
            model_id: "anthropic/claude-sonnet-4-5",
            workspace_str: "/var/lib/dyson/workspace",
            chats_str: "/var/lib/dyson/chats",
            auth_block: json!({ "type": "dangerous_no_auth" }),
            tools: Some(&tools),
        });
        assert_eq!(
            cfg["skills"]["builtin"]["tools"],
            json!(["bash", "read_file"])
        );
    }

    #[test]
    fn swarm_config_writes_empty_tool_list_for_zero_tools() {
        // Operator picked zero tools: emit `skills.builtin.tools = []`,
        // which the loader's explicit-empty-array branch reads as
        // "register no builtins".  Distinct from the omitted-skills
        // case, which means "all builtins".
        let cfg = build_swarm_config(SwarmConfigInputs {
            bind: "0.0.0.0:80",
            proxy_url: "https://dyson.example.com/llm",
            api_key: "swarm-token",
            model_id: "anthropic/claude-sonnet-4-5",
            workspace_str: "/var/lib/dyson/workspace",
            chats_str: "/var/lib/dyson/chats",
            auth_block: json!({ "type": "dangerous_no_auth" }),
            tools: Some(&[]),
        });
        assert_eq!(cfg["skills"]["builtin"]["tools"], json!([]));
    }
}
