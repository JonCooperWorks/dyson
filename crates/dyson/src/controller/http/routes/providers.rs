// ===========================================================================
// /api/providers — list providers + their models with the active one
// sorted first.  Honors the runtime model override that `post_model`
// installs so the UI's active-model label matches the next-turn build.
// ===========================================================================

use super::super::responses::{Resp, json_ok};
use super::super::state::HttpState;
use super::super::wire::ProviderDto;

pub(super) fn list(state: &HttpState) -> Resp {
    // The startup settings name an active provider + model, but the
    // operator may have switched since then via `POST /api/model` —
    // let the runtime override win so the UI's active-model label
    // matches what actually runs on the next turn.  Snapshot once so
    // the list and the active-model calculation read the same
    // settings (no cross-call torn reads if a hot-reload races this).
    let snapshot = state.settings_snapshot();
    let runtime = state
        .runtime_model
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let active_name = runtime
        .as_ref()
        .map(|(p, _)| p.clone())
        .or_else(|| crate::controller::active_provider_name(&snapshot));
    let active_model_override = runtime.as_ref().map(|(_, m)| m.clone());

    let mut dtos: Vec<ProviderDto> = snapshot
        .providers
        .iter()
        .map(|(id, pc)| {
            let is_active = active_name.as_deref() == Some(id.as_str());
            let active_model = if is_active {
                active_model_override
                    .clone()
                    .unwrap_or_else(|| snapshot.agent.model.clone())
            } else {
                pc.models.first().cloned().unwrap_or_default()
            };
            ProviderDto {
                id: id.clone(),
                name: id.clone(),
                models: pc.models.clone(),
                active_model,
                active: is_active,
            }
        })
        .collect();
    dtos.sort_by_key(|p| !p.active);
    json_ok(&dtos)
}
