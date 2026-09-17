//! Load dyson.json: discover and migrate the file, convert settings, resolve
//! credentials, then validate the model and subagent references.
mod files;
mod parsing;
mod schema;
mod secrets;
mod validation;

use crate::config::Settings;
use crate::error::Result;
use crate::secret::SecretRegistry;
pub use files::persist_model_selection;
use files::{load_and_migrate, try_discover_config};
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
/// Config files are automatically migrated in-memory before parsing.
/// Old formats (e.g. inline `agent.provider`/`api_key`) are upgraded
/// to the current schema via the migration chain in `config::migrate`.
pub fn load_settings(path: Option<&Path>) -> Result<Settings> {
    let json_root = match path {
        Some(p) => load_and_migrate(p)?,
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
