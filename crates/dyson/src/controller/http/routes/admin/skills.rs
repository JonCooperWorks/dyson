//! Package installation and live skill diagnostics.
use super::{
    HttpState, Resp, authorize_configure, bad_request, json_ok, json_status, open_workspace,
    read_json_capped,
};
use hyper::{Request, StatusCode};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Package install payload is a validated SKILL.md body plus metadata.
/// Keep it above the swarm-side skill body cap to leave JSON overhead.
const MAX_SKILL_INSTALL_BODY: usize = 96 * 1024;
#[derive(Debug, Deserialize)]
struct InstallSkillAdminBody {
    marketplace: String,
    skill: String,
    #[serde(default)]
    force: bool,
    package: crate::tool::skill_marketplace::SkillBody,
}

pub(in super::super) async fn post_skill_install(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    let body: InstallSkillAdminBody = match read_json_capped(req, MAX_SKILL_INSTALL_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let workspace = match open_workspace(&snapshot) {
        Ok(w) => Arc::new(RwLock::new(w)),
        Err(resp) => return *resp,
    };
    match crate::tool::skill_marketplace::install_skill_package_to_workspace(
        &workspace,
        body.marketplace.trim(),
        body.skill.trim(),
        body.package,
        body.force,
    )
    .await
    {
        Ok(outcome) => json_ok(&outcome),
        Err(crate::tool::skill_marketplace::SkillInstallError::AlreadyInstalled {
            current_version,
        }) => json_status(
            StatusCode::CONFLICT,
            &serde_json::json!({
                "error": "already_installed",
                "current_version": current_version,
            }),
        ),
        Err(crate::tool::skill_marketplace::SkillInstallError::Invalid(msg)) => bad_request(&msg),
        Err(crate::tool::skill_marketplace::SkillInstallError::Workspace(err)) => {
            bad_request(&format!("workspace install failed: {err}"))
        }
    }
}

pub(in super::super) async fn delete_skill(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
    skill: &str,
) -> Resp {
    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }
    let snapshot = state.settings_snapshot();
    let workspace = match open_workspace(&snapshot) {
        Ok(w) => Arc::new(RwLock::new(w)),
        Err(resp) => return *resp,
    };
    match crate::tool::skill_marketplace::remove_skill_from_workspace(&workspace, skill.trim())
        .await
    {
        Ok(outcome) => json_ok(&outcome),
        Err(crate::tool::skill_marketplace::SkillRemoveError::Invalid(msg)) => bad_request(&msg),
        Err(crate::tool::skill_marketplace::SkillRemoveError::NotInstalled) => json_status(
            StatusCode::NOT_FOUND,
            &serde_json::json!({
                "error": "skill_not_installed",
                "skill": skill,
            }),
        ),
        Err(crate::tool::skill_marketplace::SkillRemoveError::Workspace(err)) => {
            bad_request(&format!("workspace uninstall failed: {err}"))
        }
    }
}

/// Diagnostic: return the live skill / tool inventory so an operator
/// can confirm which MCP servers actually loaded after a configure
/// push.  Same configure-secret auth as `post()` (the only auth
/// surface on `/api/admin/*`).  Builds a throwaway agent off the
/// current settings so we report the actual `on_load` outcome — a
/// live `state.registry` only caches LLM clients, not skills.
pub(in super::super) async fn get_skills(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    use crate::skill::Skill;

    if let Some(resp) = authorize_configure(req.headers(), state).await {
        return resp;
    }

    // Re-load settings fresh from disk — this is the same path
    // build_agent uses, so the result reflects what an actual chat
    // turn would build with.
    let path = match state.config_path() {
        Some(p) => p.to_path_buf(),
        None => return bad_request("config_path is not set"),
    };
    let settings = match crate::config::loader::load_settings(Some(&path)) {
        Ok(s) => s,
        Err(e) => return bad_request(&format!("load_settings: {e}")),
    };

    let mut by_kind: Vec<serde_json::Value> = Vec::new();
    let mut mcp_listed: Vec<serde_json::Value> = Vec::new();
    for sk in &settings.skills {
        match sk {
            crate::config::SkillConfig::Builtin(b) => {
                by_kind.push(serde_json::json!({
                    "kind": "builtin",
                    "tools_filter": b.tools.len(),
                }));
            }
            crate::config::SkillConfig::Local(l) => {
                by_kind.push(serde_json::json!({
                    "kind": "local",
                    "name": l.name,
                    "path": l.path,
                }));
            }
            crate::config::SkillConfig::Subagent(sa) => {
                by_kind.push(serde_json::json!({
                    "kind": "subagent",
                    "agents": sa.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                }));
            }
            crate::config::SkillConfig::Mcp(m) => {
                let transport = match &m.transport {
                    crate::config::McpTransportConfig::Http { url, headers, auth } => {
                        serde_json::json!({
                            "type": "http",
                            "url": url,
                            "header_keys": headers.keys().collect::<Vec<_>>(),
                            "oauth": auth.is_some(),
                        })
                    }
                    crate::config::McpTransportConfig::Stdio { command, .. } => {
                        serde_json::json!({ "type": "stdio", "command": command })
                    }
                };
                mcp_listed.push(serde_json::json!({
                    "name": m.name,
                    "transport": transport,
                }));
            }
        }
    }

    // Try to actually load each MCP skill so we can report the
    // on_load outcome — handshake errors (the silent-skip path in
    // skill::build_skills) surface here as `loaded: false` with the
    // captured error string.  Doesn't share state with running
    // chats; just a probe.
    let mut mcp_probes: Vec<serde_json::Value> = Vec::new();
    for sk in &settings.skills {
        if let crate::config::SkillConfig::Mcp(cfg) = sk {
            let mut skill = crate::skill::mcp::McpSkill::new(*cfg.clone());
            let result = skill.on_load().await;
            mcp_probes.push(match result {
                Ok(()) => serde_json::json!({
                    "name": cfg.name,
                    "loaded": true,
                    "tools": skill.tools().len(),
                    "tool_names": skill.tools().iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
                    // Server-advertised identity + guidance from
                    // initialize.  `title` falls back to serverInfo.name
                    // when the server didn't supply a friendly title.
                    // Omitted when the server didn't advertise them so
                    // the UI can fall back to the operator alias.
                    "title": skill.server_display_name(),
                    "version": skill.server_version(),
                    "instructions": skill.server_instructions(),
                }),
                Err(e) => serde_json::json!({
                    "name": cfg.name,
                    "loaded": false,
                    "error": e.to_string(),
                }),
            });
        }
    }

    json_ok(&serde_json::json!({
        "ok": true,
        "skills": by_kind,
        "mcp_servers": mcp_listed,
        "mcp_probes": mcp_probes,
    }))
}
