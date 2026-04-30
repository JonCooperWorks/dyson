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
    json_ok(&serde_json::json!({ "name": name }))
}

fn parse_name(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("Name:"))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
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
}
