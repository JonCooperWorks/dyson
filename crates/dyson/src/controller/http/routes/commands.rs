use super::super::responses::{Resp, json_ok};
use super::super::state::HttpState;

pub(super) async fn get(state: &HttpState) -> Resp {
    let settings = state.settings_snapshot();
    json_ok(&crate::controller::slash::commands_for_settings(&settings))
}
