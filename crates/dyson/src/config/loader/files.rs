//! Configuration discovery, migration, permissions, and persistence.
use super::schema::JsonRoot;
use crate::error::{DysonError, Result};
use std::path::Path;
const MAX_CONFIG_SIZE: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a config file with a size limit to prevent DoS.
///
/// Reads the file first, then checks the length — avoids a TOCTOU race
/// between `metadata()` and `read_to_string()` where the file could be
/// swapped between the two calls.
pub(super) fn read_config_file(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| DysonError::Config(format!("cannot read config {}: {e}", path.display())))?;
    if content.len() as u64 > MAX_CONFIG_SIZE {
        return Err(DysonError::Config(format!(
            "config file {} is too large ({} bytes, max {} bytes)",
            path.display(),
            content.len(),
            MAX_CONFIG_SIZE,
        )));
    }
    tighten_config_perms(path);
    Ok(content)
}

/// Tighten dyson.json permissions to 0o600 on Unix when the file is
/// group/world readable. The file may contain literal API keys or
/// secret resolver references, so a 0o644 file is a credential-leak
/// hazard. Best-effort: warns on failure but never aborts startup.
#[cfg(unix)]
pub(super) fn tighten_config_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return;
    }
    tracing::warn!(
        path = %path.display(),
        mode = format!("{mode:o}"),
        "config file is group/world accessible; tightening to 0o600"
    );
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to tighten config permissions"
        );
    }
}

#[cfg(not(unix))]
pub(super) fn tighten_config_perms(_path: &Path) {}

/// Try to find a dyson.json in standard locations.
///
/// Attempts to read files directly instead of checking `exists()` first,
/// avoiding a TOCTOU race where the file could be swapped between the
/// existence check and the read.
pub(super) fn try_discover_config() -> Result<Option<JsonRoot>> {
    // 1. Current directory.
    let cwd_path = Path::new("dyson.json");
    if let Ok(root) = load_and_migrate(cwd_path) {
        return Ok(root);
    }

    // 2. ~/.config/dyson/dyson.json
    if let Some(home) = std::env::var_os("HOME") {
        let global_path = Path::new(&home).join(".config/dyson/dyson.json");
        if let Ok(root) = load_and_migrate(&global_path) {
            return Ok(root);
        }
    }

    Ok(None)
}

/// Read a config file, migrate it, write back if changed, and parse.
pub(super) fn load_and_migrate(path: &Path) -> Result<Option<JsonRoot>> {
    let content = read_config_file(path)?;
    let mut raw: serde_json::Value = serde_json::from_str(&content)?;
    if crate::config::migrate::migrate(&mut raw)? {
        write_back_config(path, &raw);
    }
    Ok(Some(serde_json::from_value::<JsonRoot>(raw)?))
}

/// Best-effort write migrated config back to disk.
///
/// Logs a warning on failure but does not propagate errors — the in-memory
/// migration already succeeded, so the runtime can proceed.  The file will
/// be migrated again on next load if write-back fails.
///
/// On Unix, the file permissions are set to 0o600 (owner read/write only)
/// because config files may contain API keys or secret resolver references.
pub(super) fn write_back_config(path: &Path, value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, format!("{json}\n")) {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to write migrated config back to disk"
                );
            } else {
                // Restrict permissions — config may contain secrets.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                tracing::info!(path = %path.display(), "wrote migrated config to disk");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize migrated config");
        }
    }
}

/// Persist the user's model selection by moving the chosen model to the
/// front of the provider's `models` array in the config file.
///
/// This makes the selected model the default on next startup (since
/// `default_model()` returns `models[0]`).
///
/// Best-effort: logs a warning on failure but never crashes.
pub fn persist_model_selection(config_path: &Path, provider_name: &str, model: &str) {
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "could not read config for model persistence");
            return;
        }
    };

    let mut root: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "could not parse config for model persistence");
            return;
        }
    };

    // Navigate to providers.<name>.models and move the selected model to front.
    let moved = root
        .get_mut("providers")
        .and_then(|p| p.get_mut(provider_name))
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.as_array_mut())
        .map(|models| {
            if let Some(pos) = models.iter().position(|v| v.as_str() == Some(model)) {
                if pos > 0 {
                    let val = models.remove(pos);
                    models.insert(0, val);
                    true
                } else {
                    false // already first
                }
            } else {
                false
            }
        })
        .unwrap_or(false);

    // Update agent.provider and agent.model so the switched model becomes the
    // default on next startup.
    let agent_updated = root
        .get_mut("agent")
        .and_then(|a| a.as_object_mut())
        .map(|agent| {
            let mut changed = false;
            let new_provider = serde_json::Value::String(provider_name.to_string());
            if agent.get("provider") != Some(&new_provider) {
                agent.insert("provider".to_string(), new_provider);
                changed = true;
            }
            let new_model = serde_json::Value::String(model.to_string());
            if agent.get("model") != Some(&new_model) {
                agent.insert("model".to_string(), new_model);
                changed = true;
            }
            changed
        })
        .unwrap_or(false);

    if moved || agent_updated {
        write_back_config(config_path, &root);
    }
}
