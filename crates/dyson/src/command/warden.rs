// ===========================================================================
// `dyson warden` — boot mode for running inside a CubeSandbox MicroVM
// under the dyson-orchestrator (warden).
//
// Reads the env envelope warden injects on sandbox creation:
//   - WARDEN_BEARER_TOKEN — auth secret the dyson_proxy stamps on every
//     forwarded request
//   - WARDEN_PROXY_URL    — base URL of warden's /llm provider proxy
//   - WARDEN_PROXY_TOKEN  — bearer for that proxy
//   - WARDEN_MODEL        — model id the agent talks to (e.g.
//                            "anthropic/claude-sonnet-4-5",
//                            "openai/gpt-4o"). No default — empty in
//                            warmup mode, required at instance boot.
//   - WARDEN_TASK         — free-text task description (seeded into
//                            workspace/TASK.md)
//   - WARDEN_NAME         — human-readable label
//   - WARDEN_INSTANCE_ID  — warden-side instance id
//
// Provider shape: warden's /llm proxy fronts upstream LLM APIs. We
// configure the dyson agent as an `openai`-compatible client pointed at
// `<WARDEN_PROXY_URL>/openrouter` — OpenRouter speaks the OpenAI Chat
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

pub async fn run() -> Result<()> {
    // WARDEN_BEARER_TOKEN is the per-instance auth secret warden injects on
    // create. It's NOT set during template build — Cube boots the rootfs
    // once to probe /healthz and snapshot; only post-snapshot restores
    // (instance creates) carry the env envelope. So treat the unset case
    // as "warmup" mode: bind with no inbound auth, serve /healthz, get
    // snapshotted. When warden later restarts us with the env set, the
    // bearer takes effect.
    let bearer = std::env::var("WARDEN_BEARER_TOKEN").unwrap_or_default();
    let warmup = bearer.is_empty();
    if warmup {
        tracing::warn!(
            "WARDEN_BEARER_TOKEN unset — running in template-warmup mode with \
             dangerous_no_auth on the HTTP controller. Expected during cube \
             template build; warden injects the bearer on instance create."
        );
    }

    let bind = std::env::var("DYSON_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
    let home = std::env::var("DYSON_HOME").unwrap_or_else(|_| DEFAULT_DYSON_HOME.into());
    let proxy_url = std::env::var("WARDEN_PROXY_URL").unwrap_or_default();
    let proxy_token = std::env::var("WARDEN_PROXY_TOKEN").unwrap_or_default();
    let task = std::env::var("WARDEN_TASK").unwrap_or_default();
    let name = std::env::var("WARDEN_NAME").unwrap_or_default();
    let instance_id = std::env::var("WARDEN_INSTANCE_ID").unwrap_or_default();
    let model = std::env::var("WARDEN_MODEL").unwrap_or_default();

    let home_path = PathBuf::from(&home);
    std::fs::create_dir_all(&home_path)
        .map_err(|e| DysonError::Config(format!("create dyson home {home}: {e}")))?;
    let workspace = home_path.join("workspace");
    std::fs::create_dir_all(&workspace)
        .map_err(|e| DysonError::Config(format!("create workspace {workspace:?}: {e}")))?;

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
                body.push_str(&format!("Warden instance id: {instance_id}\n"));
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

    // Provider config — warden's /llm proxy fronts the upstream LLM APIs.
    // For the smoke test the agent is never invoked; the provider just
    // needs to parse cleanly so `listen` can come up. Dyson's loader
    // refuses base_url with an empty api_key (defends against env-var
    // fallback to a non-default endpoint) — supply a placeholder when
    // warden hasn't set the proxy token (warmup).
    //
    // Dyson's loader also refuses to boot with no model set (validate_agent_model).
    // In warmup the agent never runs, so a placeholder model is fine; at
    // instance boot the operator must supply WARDEN_MODEL via the create
    // request's env (warden refuses the create otherwise — see the
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
    let provider_block = if proxy_url.is_empty() {
        json!({
            "type": "openai",
            "api_key": api_key,
            "models": [model_id]
        })
    } else {
        json!({
            "type": "openai",
            "base_url": warden_provider_base_url(&proxy_url),
            "api_key": api_key,
            "models": [model_id]
        })
    };

    // Provider key mirrors the upstream warden's /llm proxy fronts —
    // OpenRouter today.  The Dyson UI surfaces this name in its
    // provider dropdown, so naming it after the actual upstream
    // (rather than "warden") makes the source obvious to operators.
    let workspace_str = workspace.to_string_lossy();
    let cfg = json!({
        "config_version": 2,
        "providers": { "openrouter": provider_block },
        "agent": { "provider": "openrouter" },
        "controllers": [
            {
                "type": "http",
                "bind": bind,
                "auth": auth_block,
                "dangerous_no_tls": true
            }
        ],
        "workspace": { "connection_string": workspace_str },
        "skills": { "builtin": { "tools": [] } }
    });

    let cfg_path = home_path.join("dyson.json");
    let cfg_bytes = serde_json::to_vec_pretty(&cfg)
        .map_err(|e| DysonError::Config(format!("serialize dyson.json: {e}")))?;
    std::fs::write(&cfg_path, cfg_bytes)
        .map_err(|e| DysonError::Config(format!("write {cfg_path:?}: {e}")))?;

    tracing::info!(
        bind = %bind,
        instance = %instance_id,
        name = %name,
        task_set = !task.is_empty(),
        "dyson warden — starting HTTP controller"
    );

    super::listen::run(Some(cfg_path), true, None, None, None).await
}

/// Build the `providers.openrouter.base_url` value that lands in dyson.json.
///
/// `OpenAiCompatClient` appends `/v1/chat/completions` to whatever
/// `base_url` it sees, so this helper deliberately stops at `/openrouter`.
/// A base ending in `/v1` doubles up to `/openrouter/v1/v1/...`, routes to
/// OR's marketing site, and dyson surfaces the resulting non-200 as a
/// generic "upstream HTTP error" — the bug pinned by the regression test
/// `warden_provider_base_url_has_no_trailing_v1`.
fn warden_provider_base_url(proxy_url: &str) -> String {
    format!("{}/openrouter", proxy_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warden_provider_base_url_has_no_trailing_v1() {
        // The contract: warden's /llm proxy URL gets one provider segment
        // appended.  `OpenAiCompatClient::stream` then appends
        // `/v1/chat/completions` itself.  Adding `/v1` here would
        // double it up.
        assert_eq!(
            warden_provider_base_url("https://dyson.example.com/llm"),
            "https://dyson.example.com/llm/openrouter"
        );
    }

    #[test]
    fn warden_provider_base_url_strips_trailing_slash() {
        assert_eq!(
            warden_provider_base_url("https://dyson.example.com/llm/"),
            "https://dyson.example.com/llm/openrouter"
        );
    }
}
