// ===========================================================================
// /api/model — switch the LLM provider/model for one or all loaded chats.
//
// Persists to `dyson.json` AND sets a runtime override so the next
// agent built from settings (new chat, first-use hydration) inherits
// the choice.
// ===========================================================================

use std::sync::Arc;

use hyper::Request;

use super::super::responses::{
    Resp, bad_request, json_ok, not_found, read_json_capped, service_unavailable,
};
use super::super::state::{ChatHandle, HttpState, RuntimeModelSelection};
use super::super::wire::{MAX_SMALL_BODY, ModelSwitchBody};

pub(super) async fn post(req: Request<hyper::body::Incoming>, state: Arc<HttpState>) -> Resp {
    let body: ModelSwitchBody = match read_json_capped(req, MAX_SMALL_BODY).await {
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
    let selection = match RuntimeModelSelection::new(body.provider.clone(), model.clone()) {
        Ok(selection) => selection,
        Err(e) => return bad_request(&e),
    };
    let client_for_swap = match state.registry.get(selection.provider()) {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(error = %e, provider = selection.provider(), "model switch provider is not ready");
            return service_unavailable(&format!(
                "provider '{}' is not ready: {}",
                selection.provider(),
                e.sanitized_message()
            ));
        }
    };

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
            agent.swap_client(client_for_swap.clone(), &model, &provider_type);
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
    //
    // Recover from poisoning — silently skipping the assignment
    // would mean the operator's model choice is dropped on the
    // floor for the rest of the process if any prior caller
    // panicked.
    let mut slot = match state.runtime_model.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *slot = Some(selection);
    drop(slot);

    json_ok(&serde_json::json!({
        "ok": true,
        "provider": body.provider,
        "model": model,
        "swapped": swapped,
    }))
}
