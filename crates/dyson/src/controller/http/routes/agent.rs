// ===========================================================================
// /api/agent — read-only metadata about the running dyson.
//
// Surfaces the operator-set name (from `Name:` in workspace/IDENTITY.md)
// so the SPA can title the chat surface — turn header, composer
// placeholder, empty-state heading — with the user's chosen label
// instead of the literal "dyson".  Source of truth is IDENTITY.md
// because that's what the agent's system prompt also reads, and
// dyson-orchestrator's /api/admin/configure rewrites it on every name
// change pushed from the swarm UI.
// ===========================================================================

use super::super::responses::{Resp, bad_request, json_ok};
use super::super::state::HttpState;
use crate::config::{McpTransportConfig, Settings, SkillConfig};

pub(super) async fn get(state: &HttpState) -> Resp {
    let snapshot = state.settings_snapshot();
    let ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    let name = ws
        .get("IDENTITY.md")
        .as_deref()
        .and_then(parse_name)
        .unwrap_or_default();
    let inventory_settings = state
        .config_path()
        .and_then(|path| crate::config::loader::load_settings(Some(path)).ok())
        .unwrap_or_else(|| snapshot.clone());
    json_ok(&serde_json::json!({
        "name": name,
        "skills": skill_inventory(&inventory_settings),
        "state_sync": crate::swarm_state_sync::status_snapshot(),
    }))
}

fn skill_inventory(settings: &Settings) -> serde_json::Value {
    let mut builtin = Vec::new();
    let mut mcp = Vec::new();

    for skill in &settings.skills {
        match skill {
            SkillConfig::Builtin(cfg) => {
                builtin.push(serde_json::json!({
                    "tools_filter": cfg.tools.len(),
                    "tools": &cfg.tools,
                }));
            }
            SkillConfig::Mcp(cfg) => {
                mcp.push(serde_json::json!({
                    "name": &cfg.name,
                    "transport": mcp_transport_kind(&cfg.transport),
                }));
            }
            SkillConfig::Local(_) | SkillConfig::Subagent(_) => {}
        }
    }

    serde_json::json!({
        "builtin": builtin,
        "mcp": mcp,
        "denials": [],
    })
}

fn mcp_transport_kind(transport: &McpTransportConfig) -> &'static str {
    match transport {
        McpTransportConfig::Stdio { .. } => "stdio",
        McpTransportConfig::Http { .. } => "http",
    }
}

fn parse_name(body: &str) -> Option<String> {
    body.lines()
        .find_map(parse_name_line)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn parse_name_line(line: &str) -> Option<&str> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("Name:") {
        return Some(rest);
    }
    let line = line.strip_prefix("- ").unwrap_or(line);
    line.strip_prefix("**Name:**")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_name_picks_first_match() {
        let body = "# Identity\n\nName: Atlas\nSwarm instance id: u9\n";
        assert_eq!(parse_name(body), Some("Atlas".into()));
    }

    #[test]
    fn parse_name_returns_none_when_absent() {
        assert_eq!(parse_name("# Identity\n\n## Mission\n\nWatch PRs.\n"), None);
    }

    #[test]
    fn parse_name_skips_blank_value() {
        assert_eq!(parse_name("Name:   \n"), None);
    }

    #[test]
    fn parse_name_reads_markdown_identity_field() {
        let body = "# IDENTITY.md — Who Am I?\n\n- **Name:** axelrod\n";
        assert_eq!(parse_name(body), Some("axelrod".into()));
    }

    #[test]
    fn skill_inventory_lists_mcp_without_credentials() {
        let mut settings = Settings::default();
        settings
            .skills
            .push(SkillConfig::Mcp(Box::new(crate::config::McpConfig {
                name: "mcp_massive".into(),
                transport: McpTransportConfig::Http {
                    url: "http://127.0.0.1/mcp".into(),
                    headers: [("Authorization".to_string(), "Bearer secret".to_string())].into(),
                    auth: None,
                },
            })));

        let inventory = skill_inventory(&settings);
        assert_eq!(inventory["mcp"][0]["name"], "mcp_massive");
        assert_eq!(inventory["mcp"][0]["transport"], "http");
        let encoded = serde_json::to_string(&inventory).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("127.0.0.1"));
        assert!(!encoded.contains("Authorization"));
    }
}
