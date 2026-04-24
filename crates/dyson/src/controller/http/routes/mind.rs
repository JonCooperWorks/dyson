// ===========================================================================
// /api/mind, /api/mind/file — workspace listing + read + write.
//
// Backed by the shared `Workspace` so the operator and the agent see
// the same files; the agent's `workspace` tool writes through the same
// path `post_file` here uses.
// ===========================================================================

use hyper::Request;

use super::super::responses::{Resp, bad_request, json_ok, not_found, parse_query, read_json};
use super::super::state::HttpState;
use super::super::wire::MindWriteBody;

pub(super) async fn get(state: &HttpState) -> Resp {
    let snapshot = state.settings_snapshot();
    let ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    let names = ws.list_files();
    let mut files: Vec<serde_json::Value> = Vec::with_capacity(names.len());
    for name in &names {
        files.push(serde_json::json!({
            "path": name,
            "size": ws.get(name).map(|c| c.len()).unwrap_or(0),
        }));
    }
    json_ok(&serde_json::json!({
        "backend": snapshot.workspace.backend,
        "files": files,
    }))
}

pub(super) async fn get_file(state: &HttpState, query: &str) -> Resp {
    let path = match parse_query(query).into_iter().find(|(k, _)| k == "path") {
        Some((_, v)) => v,
        None => return bad_request("missing 'path' query parameter"),
    };
    let snapshot = state.settings_snapshot();
    let ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    match ws.get(&path) {
        Some(content) => json_ok(&serde_json::json!({ "path": path, "content": content })),
        None => not_found(),
    }
}

pub(super) async fn post_file(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    let body: MindWriteBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let mut ws = match crate::workspace::create_workspace(&snapshot.workspace) {
        Ok(w) => w,
        Err(e) => return bad_request(&format!("workspace open failed: {e}")),
    };
    ws.set(&body.path, &body.content);
    if let Err(e) = ws.save() {
        return bad_request(&format!("workspace save failed: {e}"));
    }
    json_ok(&serde_json::json!({ "ok": true, "path": body.path }))
}
