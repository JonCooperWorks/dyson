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

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
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
    dyson::tool::agent_secrets::set_runtime_config_from_parts(
        &proxy_url,
        &proxy_token,
        &instance_id,
    );
    dyson::swarm_cost::set_runtime_config_from_parts(&proxy_url, &proxy_token);
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
            let body = build_identity_md(&name, &instance_id, &task);
            let _ = std::fs::write(&identity, body);
        }
    }

    // Materialize the box's SSH identity into ~/.ssh from the SWARM_SSH_*
    // envelope so the agent has a stable key to push to git / SSH out with.
    // Written to the process HOME (dyson runs as root → /root/.ssh), not
    // DYSON_HOME, since ssh/git read $HOME/.ssh. Idempotent: a rotation or
    // a user-placed key always wins. Best-effort — a bad key must not block
    // boot.
    if let Err(e) = seed_ssh_key_from_env() {
        tracing::warn!(error = %e, "swarm: failed to seed ~/.ssh from SWARM_SSH_*");
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
        dyson::controller::WARMUP_PLACEHOLDER.to_string()
    } else {
        proxy_token
    };
    let model_id = if model.is_empty() {
        dyson::controller::WARMUP_PLACEHOLDER.to_string()
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
        instance_id: &instance_id,
        auth_block,
        tools: tools.as_deref(),
    });

    let cfg_path = home_path.join("dyson.json");
    let cfg_bytes = serde_json::to_vec_pretty(&cfg)
        .map_err(|e| DysonError::Config(format!("serialize dyson.json: {e}")))?;
    std::fs::write(&cfg_path, cfg_bytes)
        .map_err(|e| DysonError::Config(format!("write {cfg_path:?}: {e}")))?;

    // If swarm dropped a configure-secret preseed file into the dyson
    // home via the cube filesystem API before bringing the instance up,
    // hash it into configure_secret_hash now — before the HTTP listener
    // is bound — so /api/admin/configure has no TOFU mint window.
    match dyson::controller::http::preseed_configure_hash(&home_path) {
        Ok(true) => tracing::info!("preseed: configure secret hashed at boot"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "preseed: failed to consume configure preseed"),
    }

    // A managed-dyson always serves the browser SPA (the http controller in
    // the config above), which answers MCP `elicitation/create` prompts. Turn
    // the capability on here at boot — not only inside `HttpController::run`
    // (controller/http/mod.rs) — so it is set process-wide before any worker,
    // controller task, or agent turn runs. The http controller sets the same
    // flag, but that call is subject to start-ordering: the concurrently
    // spawned telegram controller or the state-sync worker below can drive an
    // agent turn (and evaluate the pentester preflight gate) before the http
    // task reaches its own `enable_ui`. Setting it here, synchronously and
    // before those spawns, makes the elicitation UI a deterministic property
    // of the deployment. Idempotent `AtomicBool` store, so the later call is a
    // harmless no-op.
    dyson::skill::mcp::elicitation::enable_ui();

    dyson::swarm_state_sync::spawn_worker(workspace.clone(), chats.clone(), state_sync);

    tracing::info!(
        bind = %bind,
        instance = %instance_id,
        name = %name,
        task_set = !task.is_empty(),
        "dyson swarm — starting HTTP controller"
    );

    // Swarm runs inside a Cube MicroVM (the VM IS the sandbox), so we
    // mint the bypass guard for the inner dyson — it has no OS sandbox
    // backend (bwrap / Apple Container) available.  This is the
    // structural equivalent of the operator passing
    // `--dangerous-no-sandbox` on the CLI.
    let sandbox_bypass = dyson::sandbox::sandbox_bypass_from_cli_flag(true);
    super::listen::run(Some(cfg_path), sandbox_bypass, None, None, None).await
}

/// Write the box's SSH keypair into `$HOME/.ssh` from the swarm env
/// envelope (`SWARM_SSH_PRIVATE_KEY_B64` is base64 of an OpenSSH private
/// key; `SWARM_SSH_PUBLIC_KEY` is the public line). Mirrors the sidecar's
/// `seed_ssh_key_from_env`. Idempotent: never clobbers an existing key.
fn seed_ssh_key_from_env() -> std::io::Result<()> {
    let private_b64 = std::env::var("SWARM_SSH_PRIVATE_KEY_B64").unwrap_or_default();
    if private_b64.trim().is_empty() {
        return Ok(());
    }
    let public = std::env::var("SWARM_SSH_PUBLIC_KEY").unwrap_or_default();
    // dyson runs as root in the cube; ssh/git read $HOME/.ssh.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    seed_ssh_key(Path::new(&home), &private_b64, &public)
}

fn seed_ssh_key(home_dir: &Path, private_b64: &str, public: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if private_b64.trim().is_empty() {
        return Ok(());
    }
    let ssh_dir = home_dir.join(".ssh");
    let key_path = ssh_dir.join("id_ed25519");
    if key_path.exists() {
        return Ok(());
    }
    let private = B64
        .decode(private_b64.trim())
        .map_err(std::io::Error::other)?;

    std::fs::create_dir_all(&ssh_dir)?;
    std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(&key_path, &private)?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    if !public.trim().is_empty() {
        let pub_path = ssh_dir.join("id_ed25519.pub");
        let mut body = public.trim_end().to_owned();
        body.push('\n');
        std::fs::write(&pub_path, body)?;
        std::fs::set_permissions(&pub_path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

fn build_identity_md(name: &str, instance_id: &str, task: &str) -> String {
    if looks_like_full_identity_doc(task) {
        return task.to_owned();
    }
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
    body
}

fn looks_like_full_identity_doc(body: &str) -> bool {
    let trimmed = body.trim_start();
    trimmed.starts_with("# IDENTITY.md") || trimmed.starts_with("# Identity")
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
    instance_id: &'a str,
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
    let telegram_controller = json!({
        "type": "telegram",
        "mode": "webhook",
        "allow_all_chats": true,
        "proxy": {
            "base_url": telegram_proxy_base_url(inputs.proxy_url, inputs.instance_id),
            "file_base_url": telegram_proxy_file_base_url(inputs.proxy_url, inputs.instance_id),
            "bearer": inputs.api_key
        }
    });

    let mut cfg = json!({
        "config_version": dyson::config::migrate::CURRENT_VERSION,
        "providers": providers,
        "agent": agent,
        "controllers": [
            {
                "type": "http",
                "bind": inputs.bind,
                "auth": inputs.auth_block,
                "dangerous_no_tls": true
            },
            telegram_controller
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

fn swarm_proxy_origin(proxy_url: &str) -> String {
    let base = proxy_url.trim_end_matches('/');
    base.strip_suffix("/llm").unwrap_or(base).to_owned()
}

fn telegram_proxy_base_url(proxy_url: &str, instance_id: &str) -> String {
    let origin = swarm_proxy_origin(proxy_url);
    if origin.is_empty() || instance_id.is_empty() {
        return String::new();
    }
    format!("{origin}/v1/proxy/telegram/{instance_id}")
}

fn telegram_proxy_file_base_url(proxy_url: &str, instance_id: &str) -> String {
    let origin = swarm_proxy_origin(proxy_url);
    if origin.is_empty() || instance_id.is_empty() {
        return String::new();
    }
    format!("{origin}/v1/proxy/telegram/{instance_id}/file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_identity_md_wraps_plain_task() {
        let s = build_identity_md("Bob", "u1", "Watch PRs.");
        assert!(s.contains("Name: Bob"));
        assert!(s.contains("Swarm instance id: u1"));
        assert!(s.contains("## Mission\n\nWatch PRs."));
    }

    #[test]
    fn build_identity_md_keeps_full_identity_doc_exact() {
        let full = "# IDENTITY.md — Who Am I?\n\n- **Name:** axelrod";
        assert_eq!(build_identity_md("Bob", "u1", full), full);
    }

    #[test]
    fn seeds_ssh_key_with_strict_perms_and_never_clobbers() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n";
        let b64 = B64.encode(pem.as_bytes());

        seed_ssh_key(home.path(), &b64, "ssh-ed25519 AAAA dyson-i1").unwrap();
        let key = home.path().join(".ssh/id_ed25519");
        assert_eq!(std::fs::read(&key).unwrap(), pem.as_bytes());
        assert_eq!(
            std::fs::read_to_string(home.path().join(".ssh/id_ed25519.pub")).unwrap(),
            "ssh-ed25519 AAAA dyson-i1\n"
        );
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(home.path().join(".ssh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // Idempotent: a second seed with a different key does not overwrite.
        seed_ssh_key(home.path(), &B64.encode(b"OTHER"), "ssh-ed25519 ZZZ x").unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), pem.as_bytes());
    }

    #[test]
    fn seed_ssh_key_skips_without_private_key() {
        let home = tempfile::tempdir().unwrap();
        seed_ssh_key(home.path(), "   ", "ssh-ed25519 AAAA x").unwrap();
        assert!(!home.path().join(".ssh/id_ed25519").exists());
    }

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
            instance_id: "inst-1",
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
            api_key: dyson::controller::WARMUP_PLACEHOLDER,
            model_id: dyson::controller::WARMUP_PLACEHOLDER,
            workspace_str: "/var/lib/dyson/workspace",
            chats_str: "/var/lib/dyson/chats",
            instance_id: "",
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
            instance_id: "inst-1",
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
            instance_id: "inst-1",
            auth_block: json!({ "type": "dangerous_no_auth" }),
            tools: Some(&[]),
        });
        assert_eq!(cfg["skills"]["builtin"]["tools"], json!([]));
    }
}
