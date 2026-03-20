// ===========================================================================
// Config migration — upgrades old dyson.json formats to current.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Defines a declarative migration chain that transforms raw JSON config
//   from any older version to the current version.  Each migration is a
//   sequence of well-defined `Step` operations (move, copy, set, remove,
//   bail) — no ad-hoc functions.
//
// How it works:
//   1. The loader calls `migrate(json)` before parsing.
//   2. `migrate()` reads `"config_version"` (default: 0).
//   3. It runs each migration's steps in order, skipping versions
//      below the current config version.
//   4. Steps are atomic JSON path operations — each one does exactly
//      one thing and is easy to audit.
//   5. If migration was applied, the loader writes the result back
//      to disk so the file stays current.
//
// Version detection:
//   Configs without a `"config_version"` field are treated as version 0.
//   This means existing configs written before versioning was introduced
//   enter the chain at v0 and get migrated forward automatically — no
//   manual intervention required.  After migration, `"config_version"`
//   is stamped into the file so subsequent loads skip already-applied
//   migrations.
//
// Adding a new migration:
//   1. Bump `CURRENT_VERSION`.
//   2. Add a `Migration` to the `migrations()` vec with the new steps.
//   3. That's it — the chain handles the rest.
//
// Step operations:
//   - Move(from, to)       — move a value from one path to another
//   - Copy(from, to)       — copy a value (source stays)
//   - SetString(path, val) — set a path to a string literal
//   - Remove(path)         — remove a field
//   - SkipIf(path)         — skip remaining steps if path exists
//   - BailIf(path, msg)    — error if path exists (ambiguous state)
//
// JSON paths use dot notation: "agent.provider", "providers.default.type".
// Intermediate objects are created automatically when setting/moving to
// a path that doesn't exist yet.
// ===========================================================================

use serde_json::Value;

use crate::error::{DysonError, Result};

/// Current config version.  Bump this when adding a new migration.
pub const CURRENT_VERSION: u64 = 1;

// ---------------------------------------------------------------------------
// Step — a single atomic operation in a migration.
// ---------------------------------------------------------------------------

/// A single declarative step in a migration chain.
///
/// Each step is a well-defined JSON transform that operates on dot-separated
/// paths.  No arbitrary code — every possible operation is enumerated here.
enum Step {
    /// Move a value from one path to another (removes source).
    /// No-op if source doesn't exist.
    Move(&'static str, &'static str),

    /// Copy a value from one path to another (keeps source).
    /// No-op if source doesn't exist.
    Copy(&'static str, &'static str),

    /// Set a path to a string value.  Creates intermediate objects.
    SetString(&'static str, &'static str),

    /// Remove a field at a path.  No-op if it doesn't exist.
    #[allow(dead_code)]
    Remove(&'static str),

    /// Skip all remaining steps in this migration if a path exists.
    /// Used for idempotency — if the migration target already exists,
    /// don't re-run.
    SkipIf(&'static str),

    /// Bail with an error if a path exists.
    /// Used when the config is in an ambiguous state that can't be
    /// resolved automatically.
    #[allow(dead_code)]
    BailIf(&'static str, &'static str),
}

/// A versioned migration: a description + a list of steps.
struct Migration {
    from_version: u64,
    description: &'static str,
    steps: &'static [Step],
}

// ---------------------------------------------------------------------------
// Migration definitions
// ---------------------------------------------------------------------------

/// All migrations, in order.
///
/// To add a new migration:
/// 1. Bump `CURRENT_VERSION` above.
/// 2. Add a `Migration` here with `from_version` = old CURRENT_VERSION.
/// 3. Define the steps using the Step enum.
fn migrations() -> &'static [Migration] {
    &[
        // v0 → v1: Move inline provider fields into "providers" map.
        //
        // Before: { "agent": { "provider": "anthropic", "api_key": "sk-...", "base_url": "..." } }
        // After:  { "providers": { "default": { "type": "anthropic", ... } }, "agent": { "provider": "default" } }
        Migration {
            from_version: 0,
            description: "Move agent.provider/api_key/base_url into providers map",
            steps: &[
                // If providers already exists, this config was partially migrated
                // or manually written — skip the whole migration.
                Step::SkipIf("providers"),
                // Copy provider-related fields into providers.default.
                // Copy (not move) provider type since we'll overwrite it with the name.
                Step::Copy("agent.provider", "providers.default.type"),
                Step::Copy("agent.model", "providers.default.model"),
                Step::Move("agent.api_key", "providers.default.api_key"),
                Step::Move("agent.base_url", "providers.default.base_url"),
                // Point agent.provider at the new named entry.
                Step::SetString("agent.provider", "default"),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all applicable migrations on a raw JSON config value.
///
/// Returns `true` if any migration was applied (caller should write back).
/// Errors if a migration can't be applied automatically.
pub fn migrate(root: &mut Value) -> Result<bool> {
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
        return Ok(false);
    }

    let mut current = version;
    for migration in migrations() {
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
            to = migration.from_version + 1,
            description = migration.description,
            "applying config migration"
        );

        apply_steps(root, migration.steps)?;
        current = migration.from_version + 1;
    }

    // Stamp the version.
    root["config_version"] = Value::Number(current.into());

    Ok(true)
}

// ---------------------------------------------------------------------------
// Step execution
// ---------------------------------------------------------------------------

/// Execute a list of steps against a JSON value.
fn apply_steps(root: &mut Value, steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Move(from, to) => {
                if let Some(val) = remove_path(root, from) {
                    set_path(root, to, val);
                }
            }
            Step::Copy(from, to) => {
                if let Some(val) = get_path(root, from).cloned() {
                    set_path(root, to, val);
                }
            }
            Step::SetString(path, value) => {
                set_path(root, path, Value::String((*value).into()));
            }
            Step::Remove(path) => {
                remove_path(root, path);
            }
            Step::SkipIf(path) => {
                if get_path(root, path).is_some() {
                    return Ok(());
                }
            }
            Step::BailIf(path, message) => {
                if get_path(root, path).is_some() {
                    return Err(DysonError::Config((*message).into()));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON path helpers
//
// Paths use dot notation: "a.b.c" navigates root["a"]["b"]["c"].
// ---------------------------------------------------------------------------

/// Get a reference to a value at a dot-separated path.
fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    // Don't return JSON null — treat it as missing.
    if current.is_null() {
        None
    } else {
        Some(current)
    }
}

/// Set a value at a dot-separated path, creating intermediate objects.
fn set_path(root: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for &key in &parts[..parts.len() - 1] {
        if !current.get(key).is_some_and(|v| v.is_object()) {
            current[key] = Value::Object(serde_json::Map::new());
        }
        current = current.get_mut(key).unwrap();
    }

    let last = parts[parts.len() - 1];
    current[last] = value;
}

/// Remove a value at a dot-separated path, returning it if it existed.
fn remove_path(root: &mut Value, path: &str) -> Option<Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = root;

    for &key in &parts[..parts.len() - 1] {
        current = current.get_mut(key)?;
    }

    let last = parts[parts.len() - 1];
    current.as_object_mut()?.remove(last)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Path helper tests --

    #[test]
    fn get_path_simple() {
        let root = json!({ "a": { "b": { "c": 42 } } });
        assert_eq!(get_path(&root, "a.b.c"), Some(&json!(42)));
    }

    #[test]
    fn get_path_missing() {
        let root = json!({ "a": { "b": 1 } });
        assert_eq!(get_path(&root, "a.x"), None);
    }

    #[test]
    fn get_path_null_is_none() {
        let root = json!({ "a": null });
        assert_eq!(get_path(&root, "a"), None);
    }

    #[test]
    fn set_path_creates_intermediates() {
        let mut root = json!({});
        set_path(&mut root, "a.b.c", json!("hello"));
        assert_eq!(root["a"]["b"]["c"], "hello");
    }

    #[test]
    fn remove_path_returns_value() {
        let mut root = json!({ "a": { "b": 42 } });
        assert_eq!(remove_path(&mut root, "a.b"), Some(json!(42)));
        assert!(root["a"].get("b").is_none());
    }

    // -- Migration chain tests --

    #[test]
    fn already_current_version() {
        let mut root = json!({ "config_version": CURRENT_VERSION });
        let applied = migrate(&mut root).unwrap();
        assert!(!applied);
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

        let applied = migrate(&mut root).unwrap();
        assert!(applied);

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

        // Agent cleaned up — api_key and base_url moved (not copied).
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());
        // model stays as agent override (Copy, not Move).
        assert_eq!(root["agent"]["model"], "claude-sonnet-4-20250514");
        // Non-provider fields preserved.
        assert_eq!(root["agent"]["max_iterations"], 50);

        // Version stamped.
        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn v0_to_v1_skips_when_providers_exist() {
        let mut root = json!({
            "providers": {
                "claude": { "type": "anthropic", "api_key": "sk-test" }
            },
            "agent": { "provider": "claude" }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied); // Version was still bumped (0 → 1).
        // But providers map is unchanged — SkipIf stopped the steps.
        assert!(root["providers"]["claude"].is_object());
        assert!(root["providers"].get("default").is_none());
    }

    #[test]
    fn v0_to_v1_handles_no_agent() {
        let mut root = json!({ "controllers": [{ "type": "terminal" }] });
        let applied = migrate(&mut root).unwrap();
        assert!(applied); // Version bumped.
        // No providers created since there was nothing to migrate.
        assert!(root.get("providers").is_none());
    }

    #[test]
    fn v0_to_v1_handles_missing_optional_fields() {
        // Only provider, no api_key or base_url.
        let mut root = json!({
            "agent": { "provider": "claude-code" }
        });

        migrate(&mut root).unwrap();

        assert_eq!(root["providers"]["default"]["type"], "claude-code");
        // Missing fields just don't appear in the provider.
        assert!(root["providers"]["default"].get("api_key").is_none());
        assert!(root["providers"]["default"].get("base_url").is_none());
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

        // Secret reference moved as-is (it's just a JSON value).
        assert_eq!(
            root["providers"]["default"]["api_key"]["resolver"],
            "insecure_env"
        );
        assert_eq!(
            root["providers"]["default"]["api_key"]["name"],
            "OPENAI_API_KEY"
        );
    }

    // -- Step operation tests --

    #[test]
    fn step_bail_if_errors() {
        let mut root = json!({ "x": 1 });
        let steps = &[Step::BailIf("x", "x exists and shouldn't")];
        let err = apply_steps(&mut root, steps).unwrap_err();
        assert!(err.to_string().contains("x exists"));
    }

    #[test]
    fn step_bail_if_passes_when_missing() {
        let mut root = json!({});
        let steps = &[Step::BailIf("x", "should not fire")];
        apply_steps(&mut root, steps).unwrap();
    }

    #[test]
    fn step_skip_if_skips_remaining() {
        let mut root = json!({ "x": 1, "y": 2 });
        let steps = &[
            Step::SkipIf("x"),
            Step::Remove("y"), // Should not execute.
        ];
        apply_steps(&mut root, steps).unwrap();
        assert_eq!(root["y"], 2); // y still there.
    }

    #[test]
    fn step_move_removes_source() {
        let mut root = json!({ "a": "hello" });
        let steps = &[Step::Move("a", "b")];
        apply_steps(&mut root, steps).unwrap();
        assert_eq!(root["b"], "hello");
        assert!(root.get("a").is_none());
    }

    #[test]
    fn step_copy_keeps_source() {
        let mut root = json!({ "a": "hello" });
        let steps = &[Step::Copy("a", "b")];
        apply_steps(&mut root, steps).unwrap();
        assert_eq!(root["a"], "hello");
        assert_eq!(root["b"], "hello");
    }
}
