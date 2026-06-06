// ===========================================================================
// /api/mcp/servers — live probe of every configured MCP server.
//
// /api/agent's `skills.mcp` reflects static dyson.json config (server
// alias + transport kind).  This route does what `/api/admin/skills`
// does — actually connects to each server, sends `initialize`, captures
// `serverInfo.title`, `instructions`, and the tool list — but with the
// SPA's regular `/api/*` auth instead of the admin configure-secret,
// so the chat UI can render server descriptions in the MCP chip
// tooltip and the Mind > MCP panel.
//
// Probes run concurrently with a per-server timeout so one slow or
// hung server can't block the whole call.  The handshake spins up a
// throwaway connection that is dropped at the end of the call —
// live chat sessions reconnect on their own.
// ===========================================================================

use std::time::Duration;

use super::super::responses::{Resp, json_ok};
use super::super::state::HttpState;
use crate::config::SkillConfig;
use crate::skill::Skill;

/// Per-server probe budget.  Most MCP servers initialize in under
/// 200 ms; 5 s is forgiving without making the route feel hung when
/// the SPA loads.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn list_servers(state: &HttpState) -> Resp {
    let settings = state
        .config_path()
        .and_then(|path| crate::config::loader::load_settings(Some(path)).ok())
        .unwrap_or_else(|| state.settings_snapshot());

    let configs: Vec<_> = settings
        .skills
        .iter()
        .filter_map(|s| match s {
            SkillConfig::Mcp(cfg) => Some((**cfg).clone()),
            _ => None,
        })
        .collect();

    let probes = configs.into_iter().map(|cfg| async move {
        let alias = cfg.name.clone();
        let mut skill = crate::skill::mcp::McpSkill::new(cfg);
        match tokio::time::timeout(PROBE_TIMEOUT, skill.on_load()).await {
            Ok(Ok(())) => serde_json::json!({
                "name": alias,
                "loaded": true,
                // serverInfo.title (fallback: serverInfo.name).  None
                // when the server didn't advertise serverInfo at all,
                // in which case the SPA falls back to the alias.
                "title": skill.server_display_name(),
                "version": skill.server_version(),
                "instructions": skill.server_instructions(),
                "tools": skill.tools().len(),
                "tool_names": skill
                    .tools()
                    .iter()
                    .map(|t| t.name().to_string())
                    .collect::<Vec<_>>(),
            }),
            Ok(Err(e)) => serde_json::json!({
                "name": alias,
                "loaded": false,
                "error": e.to_string(),
            }),
            Err(_) => serde_json::json!({
                "name": alias,
                "loaded": false,
                "error": format!("probe timed out after {}s", PROBE_TIMEOUT.as_secs()),
            }),
        }
    });

    let results: Vec<serde_json::Value> = futures_util::future::join_all(probes).await;
    json_ok(&serde_json::json!({ "servers": results }))
}
