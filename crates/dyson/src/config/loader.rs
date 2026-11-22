//! Load dyson.json: discover the file, check its `config_version`, convert settings, resolve
//! credentials, then validate the model and subagent references.
mod files;
mod parsing;
mod schema;
mod secrets;
mod validation;

use crate::config::Settings;
use crate::secret::SecretRegistry;
use dyson_core::error::Result;
pub use files::persist_model_selection;
use files::{load_config, try_discover_config};
use parsing::build_settings;
use secrets::resolve_api_keys;
use std::path::Path;
use validation::validate_agent_model;
pub use validation::validate_subagent_configs;

/// Load settings from a dyson.json file, falling back to defaults.
///
/// ## Resolution order
///
/// 1. If `path` is `Some`, load that exact file (error if missing).
/// 2. Try `./dyson.json` in the current directory.
/// 3. Try `~/.config/dyson/dyson.json`.
/// 4. No file found → use built-in defaults.
///
/// A file must carry `config_version` equal to [`crate::config::CONFIG_VERSION`]
/// and use only current keys; anything else is refused with an error.
pub fn load_settings(path: Option<&Path>) -> Result<Settings> {
    let json_root = match path {
        Some(p) => load_config(p)?,
        None => try_discover_config()?,
    };

    let secrets = SecretRegistry::default();
    let mut settings = build_settings(json_root, &secrets);

    resolve_api_keys(&mut settings, &secrets)?;
    validate_agent_model(&settings)?;
    validate_subagent_configs(&settings)?;

    Ok(settings)
}

#[cfg(test)]
mod tests;
