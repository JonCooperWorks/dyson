// ===========================================================================
// Config migration — upgrades old dyson.json formats to current.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines a chain of versioned migrations that transform a raw JSON
//   config value in-place before the loader parses it.  Each migration
//   checks whether it applies, transforms the JSON, and bumps the
//   version.  Migrations that can't be applied automatically return
//   an error describing what the user needs to fix manually.
//
// How it works:
//   1. The loader calls `migrate(json)` before parsing.
//   2. `migrate()` reads the `"config_version"` field (default: 0).
//   3. It runs each migration in order, skipping those below the
//      current version.
//   4. Each migration either succeeds (mutates the JSON) or returns
//      `Err` with a human-readable message.
//   5. The final JSON is at the latest version and ready for parsing.
//
// Adding a new migration:
//   1. Add a new function `fn migrate_vN_to_vM(root: &mut Value) -> Result<()>`
//   2. Add it to the `MIGRATIONS` array with the source version.
//   3. That's it — the chain handles the rest.
//
// Design principles:
//   - Migrations are pure JSON transforms — no I/O, no side effects.
//   - Each migration is idempotent (safe to re-run on already-migrated JSON).
//   - Unresolvable changes bail with a clear error message.
//   - The original file is never modified — migration happens in memory.
//     The user can run `dyson config upgrade` to write back (future).
// ===========================================================================

use serde_json::Value;

use crate::error::{DysonError, Result};

/// Current config version.  Bump this when adding a new migration.
pub const CURRENT_VERSION: u64 = 1;

/// A single migration step.
struct Migration {
    /// The version this migration upgrades FROM.
    from_version: u64,
    /// Human-readable description.
    description: &'static str,
    /// The migration function.  Mutates the JSON in-place.
    apply: fn(&mut Value) -> Result<()>,
}

/// All migrations, in order.  Each upgrades from `from_version` to
/// `from_version + 1`.
const MIGRATIONS: &[Migration] = &[Migration {
    from_version: 0,
    description: "Move agent.provider/api_key/base_url into providers map",
    apply: migrate_v0_to_v1,
}];

/// Run all applicable migrations on a raw JSON config value.
///
/// Returns the (possibly mutated) JSON at the latest version.
/// Errors if a migration can't be applied automatically.
pub fn migrate(root: &mut Value) -> Result<()> {
    let version = root
        .get("config_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if version > CURRENT_VERSION {
        return Err(DysonError::Config(format!(
            "config_version {version} is newer than this version of Dyson (max {CURRENT_VERSION}).  \
             Please upgrade Dyson."
        )));
    }

    if version == CURRENT_VERSION {
        return Ok(());
    }

    let mut current = version;
    for migration in MIGRATIONS {
        if migration.from_version < current {
            continue;
        }
        if migration.from_version != current {
            return Err(DysonError::Config(format!(
                "migration gap: config is at version {current} but next migration is from {}",
                migration.from_version,
            )));
        }

        tracing::info!(
            from = migration.from_version,
            description = migration.description,
            "applying config migration"
        );

        (migration.apply)(root)?;
        current = migration.from_version + 1;
    }

    // Stamp the version.
    root["config_version"] = Value::Number(current.into());

    Ok(())
}

// ---------------------------------------------------------------------------
// v0 → v1: Move inline provider fields into "providers" map.
//
// Before (v0):
//   { "agent": { "provider": "anthropic", "api_key": "sk-...", "base_url": "..." } }
//
// After (v1):
//   { "providers": { "default": { "type": "anthropic", "api_key": "sk-...", "base_url": "..." } },
//     "agent": { "provider": "default" } }
//
// Bail conditions:
//   - "providers" key already exists (ambiguous — user may have partially migrated)
// ---------------------------------------------------------------------------

fn migrate_v0_to_v1(root: &mut Value) -> Result<()> {
    // Already has providers — nothing to do (idempotent).
    if root.get("providers").is_some() {
        return Ok(());
    }

    let agent = match root.get("agent") {
        Some(Value::Object(_)) => root["agent"].clone(),
        _ => return Ok(()), // No agent block — nothing to migrate.
    };

    // Extract provider-specific fields from agent.
    let provider_type = agent
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("anthropic");

    let has_inline_fields = agent.get("api_key").is_some()
        || agent.get("base_url").is_some()
        || agent.get("provider").is_some();

    if !has_inline_fields {
        return Ok(()); // Nothing to migrate.
    }

    // Build the provider entry.
    let mut provider_entry = serde_json::Map::new();
    provider_entry.insert(
        "type".into(),
        Value::String(provider_type.to_string()),
    );

    if let Some(model) = agent.get("model") {
        provider_entry.insert("model".into(), model.clone());
    }
    if let Some(api_key) = agent.get("api_key") {
        provider_entry.insert("api_key".into(), api_key.clone());
    }
    if let Some(base_url) = agent.get("base_url") {
        provider_entry.insert("base_url".into(), base_url.clone());
    }

    // Create the providers map with a "default" entry.
    let mut providers = serde_json::Map::new();
    providers.insert("default".into(), Value::Object(provider_entry));
    root["providers"] = Value::Object(providers);

    // Clean up agent: remove migrated fields, set provider to name ref.
    if let Some(agent_obj) = root.get_mut("agent").and_then(|v| v.as_object_mut()) {
        agent_obj.remove("api_key");
        agent_obj.remove("base_url");
        agent_obj.insert("provider".into(), Value::String("default".into()));
        // Keep "model" on agent as an override (the provider also has it,
        // but this preserves any explicit agent-level model preference).
    }

    tracing::info!("migrated v0→v1: moved inline provider to providers.default");
    Ok(())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn already_current_version() {
        let mut root = json!({ "config_version": CURRENT_VERSION });
        migrate(&mut root).unwrap();
        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn future_version_errors() {
        let mut root = json!({ "config_version": CURRENT_VERSION + 1 });
        let err = migrate(&mut root).unwrap_err();
        assert!(err.to_string().contains("newer than this version"));
    }

    #[test]
    fn v0_to_v1_moves_inline_provider() {
        let mut root = json!({
            "agent": {
                "provider": "anthropic",
                "model": "claude-sonnet-4-20250514",
                "api_key": "sk-test",
                "base_url": "https://api.example.com",
                "max_iterations": 50
            }
        });

        migrate(&mut root).unwrap();

        // Provider map created.
        assert!(root["providers"]["default"].is_object());
        assert_eq!(root["providers"]["default"]["type"], "anthropic");
        assert_eq!(root["providers"]["default"]["api_key"], "sk-test");
        assert_eq!(
            root["providers"]["default"]["base_url"],
            "https://api.example.com"
        );
        assert_eq!(
            root["providers"]["default"]["model"],
            "claude-sonnet-4-20250514"
        );

        // Agent cleaned up.
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());
        // model stays as agent override.
        assert_eq!(root["agent"]["model"], "claude-sonnet-4-20250514");
        // Non-provider fields preserved.
        assert_eq!(root["agent"]["max_iterations"], 50);

        // Version stamped.
        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn v0_to_v1_noop_when_providers_exist() {
        let mut root = json!({
            "providers": {
                "claude": { "type": "anthropic", "api_key": "sk-test" }
            },
            "agent": { "provider": "claude" }
        });

        migrate(&mut root).unwrap();
        // Providers unchanged.
        assert!(root["providers"]["claude"].is_object());
        assert!(root["providers"].get("default").is_none());
    }

    #[test]
    fn v0_to_v1_noop_when_no_agent() {
        let mut root = json!({ "controllers": [{ "type": "terminal" }] });
        migrate(&mut root).unwrap();
        assert!(root.get("providers").is_none());
    }

    #[test]
    fn v0_to_v1_minimal_agent_no_inline_fields() {
        let mut root = json!({
            "agent": { "max_iterations": 10 }
        });
        migrate(&mut root).unwrap();
        // No inline fields to migrate — providers not created.
        assert!(root.get("providers").is_none());
    }

    #[test]
    fn v0_to_v1_secret_reference_preserved() {
        let mut root = json!({
            "agent": {
                "provider": "openai",
                "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" }
            }
        });

        migrate(&mut root).unwrap();

        // Secret reference is preserved as-is in the provider.
        assert_eq!(
            root["providers"]["default"]["api_key"]["resolver"],
            "insecure_env"
        );
        assert_eq!(
            root["providers"]["default"]["api_key"]["name"],
            "OPENAI_API_KEY"
        );
    }
}
