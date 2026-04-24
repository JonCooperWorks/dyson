// ===========================================================================
// /api/model — switch the LLM provider/model for one or all loaded chats.
//
// Persists to `dyson.json` AND sets a runtime override so the next
// agent built from settings (new chat, first-use hydration) inherits
// the choice.
// ===========================================================================

use std::sync::Arc;

use hyper::Request;

use super::super::responses::{Resp, bad_request, json_ok, not_found, read_json};
use super::super::state::{ChatHandle, HttpState};
use super::super::wire::ModelSwitchBody;

pub(super) async fn post(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    let body: ModelSwitchBody = match read_json(req).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };
    let snapshot = state.settings_snapshot();
    let provider_cfg = match snapshot.providers.get(&body.provider) {
        Some(c) => c,
        None => return bad_request(&format!("unknown provider '{}'", body.provider)),
    };
    let model = body
        .model
        .clone()
        .or_else(|| provider_cfg.models.first().cloned())
        .unwrap_or_default();
    if model.is_empty() {
        return bad_request("provider has no configured models");
    }
    let provider_type = provider_cfg.provider_type.clone();

    let chats = state.chats.lock().await;
    let targets: Vec<Arc<ChatHandle>> = match body.chat_id {
        Some(id) => match chats.get(&id) {
            Some(h) => vec![Arc::clone(h)],
            None => return not_found(),
        },
        None => chats.values().cloned().collect(),
    };
    drop(chats);

    let mut swapped = 0usize;
    for handle in targets {
        let mut guard = handle.agent.lock().await;
        if let Some(agent) = guard.as_mut() {
            let client = state
                .registry
                .get(&body.provider)
                .unwrap_or_else(|_| state.registry.get_default());
            agent.swap_client(client, &model, &provider_type);
            swapped += 1;
        }
    }

    // Persist to dyson.json so the choice survives a restart and a
    // new conversation picks it up as the default.  Before this
    // fix the web UI silently lost its model switch the next time
    // any code rebuilt an agent from `Settings` — Telegram's
    // `/model` command already writes through the same helper, so
    // without this HTTP the two controllers fought each other.
    if let Some(cp) = state.config_path.as_ref() {
        crate::config::loader::persist_model_selection(cp, &body.provider, &model);
    }
    // In-memory override so the *next* agent this process builds
    // (new chat, first-use hydration, etc.) also picks up the
    // choice without needing a restart.  `state.settings` is
    // frozen from startup; without this, post_model would only
    // affect already-running agents.
    if let Ok(mut slot) = state.runtime_model.lock() {
        *slot = Some((body.provider.clone(), model.clone()));
    }

    json_ok(&serde_json::json!({
        "ok": true,
        "provider": body.provider,
        "model": model,
        "swapped": swapped,
    }))
}
