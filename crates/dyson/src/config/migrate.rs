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
pub const CURRENT_VERSION: u64 = 3;

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

    /// For each child object under `parent_path`, remove `old_key` and
    /// insert `new_key` with the value wrapped in a JSON array.
    /// If the value is already an array, it's moved as-is.
    /// No-op for children that don't have `old_key`.
    ///
    /// Example: RenameWrapArray("providers", "model", "models")
    ///   { "providers": { "a": { "model": "x" } } }
    ///   → { "providers": { "a": { "models": ["x"] } } }
    RenameWrapArray(&'static str, &'static str, &'static str),
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
const fn migrations() -> &'static [Migration] {
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
        // v1 → v2: Rename providers.*.model (string) to providers.*.models (array).
        //
        // Before: { "providers": { "claude": { "type": "anthropic", "model": "claude-sonnet-4" } } }
        // After:  { "providers": { "claude": { "type": "anthropic", "models": ["claude-sonnet-4"] } } }
        //
        // Also removes agent.model since the model is now selected from the
        // provider's models list, and agent.model was just a copy of it.
        Migration {
            from_version: 1,
            description: "Rename providers.*.model to providers.*.models (array)",
            steps: &[
                Step::RenameWrapArray("providers", "model", "models"),
                Step::Remove("agent.model"),
            ],
        },
        // v2 → v3: Marker migration documenting the new optional
        // `allowed_sub` field on `controllers[].auth` for OIDC.  When
        // set, the controller refuses any JWT whose `sub` claim
        // doesn't match — locking the instance to a single user when
        // the OIDC `client_id` is shared across an enterprise.  The
        // same gate applies to SSE one-shot tickets.
        //
        // No structural changes: the field is `Option<String>` with a
        // serde default, so existing v2 configs continue to load
        // unchanged.  The version bump just stamps the schema so
        // future tooling (and `journalctl`) can identify which
        // capability set the operator's config is aware of.
        Migration {
            from_version: 2,
            description: "OIDC controllers may now declare allowed_sub to lock the controller to a single user",
            steps: &[],
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
        .and_then(serde_json::Value::as_u64)
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
            Step::RenameWrapArray(parent_path, old_key, new_key) => {
                if let Some(parent) = get_path_mut(root, parent_path)
                    && let Some(map) = parent.as_object_mut()
                {
                    for (_name, child) in map.iter_mut() {
                        if let Some(obj) = child.as_object_mut()
                            && let Some(val) = obj.remove(*old_key)
                        {
                            let wrapped = if val.is_array() {
                                val
                            } else {
                                Value::Array(vec![val])
                            };
                            obj.insert((*new_key).into(), wrapped);
                        }
                    }
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

/// Get a mutable reference to a value at a dot-separated path.
fn get_path_mut<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut current = root;
    for key in path.split('.') {
        current = current.get_mut(key)?;
    }
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
        if !current.get(key).is_some_and(serde_json::Value::is_object) {
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

        // Provider map created (v1 creates it, v2 converts model → models).
        assert!(root["providers"]["default"].is_object());
        assert_eq!(root["providers"]["default"]["type"], "anthropic");
        assert_eq!(root["providers"]["default"]["api_key"], "sk-test");
        assert_eq!(
            root["providers"]["default"]["base_url"],
            "https://api.example.com"
        );
        // v2 migration wraps model in array and renames to models.
        assert_eq!(
            root["providers"]["default"]["models"],
            json!(["claude-sonnet-4-20250514"])
        );
        assert!(root["providers"]["default"].get("model").is_none());

        // Agent cleaned up — api_key and base_url moved (not copied).
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());
        // v2 migration removes agent.model.
        assert!(root["agent"].get("model").is_none());
        // Non-provider fields preserved.
        assert_eq!(root["agent"]["max_iterations"], 50);

        // Version stamped.
        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn v0_to_v1_skips_when_providers_exist() {
        let mut root = json!({
            "providers": {
                "claude": { "type": "anthropic", "model": "claude-sonnet-4", "api_key": "sk-test" }
            },
            "agent": { "provider": "claude" }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied); // Version was still bumped (0 → 2).
        // v1 SkipIf stopped the provider-move steps.
        assert!(root["providers"]["claude"].is_object());
        assert!(root["providers"].get("default").is_none());
        // But v2 still ran — model → models.
        assert_eq!(
            root["providers"]["claude"]["models"],
            json!(["claude-sonnet-4"])
        );
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

    // -- v1 → v2 migration tests --

    #[test]
    fn v1_to_v2_wraps_model_in_array() {
        let mut root = json!({
            "config_version": 1,
            "providers": {
                "claude": { "type": "anthropic", "model": "claude-sonnet-4" },
                "gpt": { "type": "openai", "model": "gpt-4o" }
            },
            "agent": { "provider": "claude", "model": "claude-sonnet-4" }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);

        // Both providers have models arrays.
        assert_eq!(
            root["providers"]["claude"]["models"],
            json!(["claude-sonnet-4"])
        );
        assert_eq!(root["providers"]["gpt"]["models"], json!(["gpt-4o"]));

        // Old model field removed.
        assert!(root["providers"]["claude"].get("model").is_none());
        assert!(root["providers"]["gpt"].get("model").is_none());

        // agent.model removed.
        assert!(root["agent"].get("model").is_none());

        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn v1_to_v2_no_model_field_is_fine() {
        let mut root = json!({
            "config_version": 1,
            "providers": {
                "cc": { "type": "claude-code" }
            }
        });

        migrate(&mut root).unwrap();

        // No model → no models field added (it's a no-op).
        assert!(root["providers"]["cc"].get("models").is_none());
        assert_eq!(root["config_version"], CURRENT_VERSION);
    }

    #[test]
    fn v1_to_v2_already_array_preserved() {
        let mut root = json!({
            "config_version": 1,
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "model": ["claude-sonnet-4", "claude-opus-4"]
                }
            }
        });

        migrate(&mut root).unwrap();

        // Array value moved as-is, not double-wrapped.
        assert_eq!(
            root["providers"]["claude"]["models"],
            json!(["claude-sonnet-4", "claude-opus-4"])
        );
    }

    // -- v0 → current (full chain) tests --

    #[test]
    fn v0_to_current_full_chain() {
        // Simulates a real v0 config with inline provider fields.
        // Should pass through v0→v1 (extract providers) then v1→v2 (model→models).
        let mut root = json!({
            "agent": {
                "provider": "anthropic",
                "model": "claude-sonnet-4-20250514",
                "api_key": "sk-ant-test",
                "base_url": "https://api.anthropic.com",
                "max_iterations": 30,
                "system_prompt": "You are helpful."
            }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);
        assert_eq!(root["config_version"], CURRENT_VERSION);

        // v1 created the providers map.
        assert!(root["providers"]["default"].is_object());
        assert_eq!(root["providers"]["default"]["type"], "anthropic");
        assert_eq!(root["providers"]["default"]["api_key"], "sk-ant-test");
        assert_eq!(
            root["providers"]["default"]["base_url"],
            "https://api.anthropic.com"
        );

        // v2 wrapped model into models array.
        assert_eq!(
            root["providers"]["default"]["models"],
            json!(["claude-sonnet-4-20250514"])
        );
        assert!(root["providers"]["default"].get("model").is_none());

        // agent.provider points to "default", agent.model removed by v2.
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("model").is_none());
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());

        // Non-provider agent fields preserved.
        assert_eq!(root["agent"]["max_iterations"], 30);
        assert_eq!(root["agent"]["system_prompt"], "You are helpful.");
    }

    #[test]
    fn v0_to_current_realistic_multi_provider() {
        // A v0 config that already has a providers map (manually written).
        // v0→v1 SkipIf fires, then v1→v2 migrates model→models.
        let mut root = json!({
            "providers": {
                "claude": {
                    "type": "anthropic",
                    "model": "claude-sonnet-4-20250514",
                    "api_key": "sk-ant"
                },
                "gpt": {
                    "type": "openai",
                    "model": "gpt-4o",
                    "api_key": { "resolver": "insecure_env", "name": "OPENAI_API_KEY" }
                },
                "local": {
                    "type": "claude-code"
                }
            },
            "agent": {
                "provider": "claude",
                "model": "claude-opus-4-20250514"
            },
            "controllers": [{ "type": "terminal" }]
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);
        assert_eq!(root["config_version"], CURRENT_VERSION);

        // v1 skipped (providers already exists), but v2 ran.
        assert_eq!(
            root["providers"]["claude"]["models"],
            json!(["claude-sonnet-4-20250514"])
        );
        assert_eq!(root["providers"]["gpt"]["models"], json!(["gpt-4o"]));
        // local had no model → no models field.
        assert!(root["providers"]["local"].get("models").is_none());

        // Secret references preserved through migration.
        assert_eq!(
            root["providers"]["gpt"]["api_key"]["resolver"],
            "insecure_env"
        );

        // agent.model removed by v2.
        assert!(root["agent"].get("model").is_none());
        // agent.provider preserved.
        assert_eq!(root["agent"]["provider"], "claude");

        // Non-provider config preserved.
        assert_eq!(root["controllers"][0]["type"], "terminal");
    }

    #[test]
    fn v1_to_v2_realistic_config() {
        // A complete v1 config — the format users would have after v0→v1 migration.
        let mut root = json!({
            "config_version": 1,
            "providers": {
                "default": {
                    "type": "anthropic",
                    "model": "claude-sonnet-4-20250514",
                    "api_key": "sk-ant-test"
                }
            },
            "agent": {
                "provider": "default",
                "model": "claude-sonnet-4-20250514",
                "max_iterations": 20,
                "max_tokens": 8192,
                "system_prompt": "You are Dyson."
            },
            "skills": { "builtin": { "tools": ["bash"] } },
            "controllers": [{ "type": "terminal" }],
            "sandbox": { "disabled": [] },
            "workspace": { "backend": "filesystem", "connection_string": "~/.dyson" }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);
        assert_eq!(root["config_version"], CURRENT_VERSION);

        // Provider model wrapped.
        assert_eq!(
            root["providers"]["default"]["models"],
            json!(["claude-sonnet-4-20250514"])
        );
        assert!(root["providers"]["default"].get("model").is_none());

        // agent.model removed, everything else preserved.
        assert!(root["agent"].get("model").is_none());
        assert_eq!(root["agent"]["provider"], "default");
        assert_eq!(root["agent"]["max_iterations"], 20);
        assert_eq!(root["agent"]["max_tokens"], 8192);
        assert_eq!(root["agent"]["system_prompt"], "You are Dyson.");

        // All other top-level sections untouched.
        assert_eq!(root["skills"]["builtin"]["tools"][0], "bash");
        assert_eq!(root["controllers"][0]["type"], "terminal");
        assert_eq!(root["workspace"]["backend"], "filesystem");
    }

    #[test]
    fn v0_to_current_dyson_json() {
        // The actual dyson.json from the project directory.
        let mut root = json!({
            "agent": {
                "provider": "claude-code",
                "model": "opus",
                "max_iterations": 20,
                "max_tokens": 8192
            },
            "workspace": {
                "path": "~/.dyson"
            },
            "controllers": [
                {
                    "type": "telegram",
                    "bot_token": "fake-token",
                    "allowed_chat_ids": ["2102424765"]
                }
            ],
            "skills": {
                "builtin": { "tools": [] }
            }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);
        assert_eq!(root["config_version"], CURRENT_VERSION);

        // v1: inline provider extracted into providers.default.
        assert_eq!(root["providers"]["default"]["type"], "claude-code");
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());

        // v2: model wrapped into models array, agent.model removed.
        assert_eq!(root["providers"]["default"]["models"], json!(["opus"]));
        assert!(root["providers"]["default"].get("model").is_none());
        assert!(root["agent"].get("model").is_none());

        // Non-provider agent fields preserved.
        assert_eq!(root["agent"]["max_iterations"], 20);
        assert_eq!(root["agent"]["max_tokens"], 8192);

        // Other sections untouched.
        assert_eq!(root["workspace"]["path"], "~/.dyson");
        assert_eq!(root["controllers"][0]["type"], "telegram");
        assert_eq!(root["skills"]["builtin"]["tools"], json!([]));
    }

    #[test]
    fn v0_to_current_dyson_local_json() {
        // The actual dyson-local.json from the project directory.
        let mut root = json!({
            "agent": {
                "provider": "openai",
                "model": "phi-4",
                "base_url": "http://localhost:8080",
                "api_key": "not-needed",
                "max_iterations": 20,
                "max_tokens": 8192
            },
            "workspace": {
                "path": "~/.dyson"
            },
            "controllers": [
                {
                    "type": "telegram",
                    "bot_token": "fake-token",
                    "allowed_chat_ids": ["2102424765"]
                }
            ],
            "skills": {
                "builtin": { "tools": [] }
            }
        });

        let applied = migrate(&mut root).unwrap();
        assert!(applied);
        assert_eq!(root["config_version"], CURRENT_VERSION);

        // v1: inline provider extracted into providers.default.
        assert_eq!(root["providers"]["default"]["type"], "openai");
        assert_eq!(root["providers"]["default"]["api_key"], "not-needed");
        assert_eq!(
            root["providers"]["default"]["base_url"],
            "http://localhost:8080"
        );

        // v2: model wrapped into models array, agent.model removed.
        assert_eq!(root["providers"]["default"]["models"], json!(["phi-4"]));
        assert!(root["providers"]["default"].get("model").is_none());
        assert!(root["agent"].get("model").is_none());

        // agent points to "default", inline fields removed.
        assert_eq!(root["agent"]["provider"], "default");
        assert!(root["agent"].get("api_key").is_none());
        assert!(root["agent"].get("base_url").is_none());
        assert_eq!(root["agent"]["max_iterations"], 20);
        assert_eq!(root["agent"]["max_tokens"], 8192);
    }

    #[test]
    fn v2_to_v3_marker_is_a_pure_version_bump() {
        // v2 → v3 is a marker migration: the `allowed_sub` OIDC
        // field is optional and serde-default, so no structural
        // change is required.  The migration just bumps the
        // stamped version so a journalctl reader can tell which
        // capability set the operator's config is aware of.
        let mut root = json!({
            "config_version": 2,
            "providers": {
                "default": { "type": "anthropic", "models": ["claude-sonnet-4"] }
            },
            "agent": { "provider": "default" },
            "controllers": [{
                "type": "http",
                "config": {
                    "bind": "0.0.0.0:7878",
                    "auth": {
                        "type": "oidc",
                        "issuer": "https://idp.example.com",
                        "audience": "dyson-web"
                    }
                }
            }]
        });
        let original = root.clone();
        let applied = migrate(&mut root).unwrap();
        assert!(applied, "version should bump 2 → 3");
        assert_eq!(root["config_version"], CURRENT_VERSION);
        assert_eq!(CURRENT_VERSION, 3);

        // Everything else byte-for-byte identical (modulo the version stamp).
        let mut without_version = root.clone();
        without_version
            .as_object_mut()
            .unwrap()
            .remove("config_version");
        let mut original_without_version = original.clone();
        original_without_version
            .as_object_mut()
            .unwrap()
            .remove("config_version");
        assert_eq!(without_version, original_without_version);
    }

    #[test]
    fn v2_to_v3_preserves_allowed_sub_when_set() {
        // An operator who has already added `allowed_sub` to a v2
        // config (manual edit) must round-trip cleanly through the
        // v3 marker without losing the field.
        let mut root = json!({
            "config_version": 2,
            "controllers": [{
                "type": "http",
                "config": {
                    "auth": {
                        "type": "oidc",
                        "issuer": "https://idp.example.com",
                        "audience": "dyson-web",
                        "allowed_sub": "alice@example.com"
                    }
                }
            }]
        });
        migrate(&mut root).unwrap();
        assert_eq!(
            root["controllers"][0]["config"]["auth"]["allowed_sub"],
            "alice@example.com"
        );
    }

    #[test]
    fn step_rename_wrap_array() {
        let mut root = json!({
            "items": {
                "a": { "val": "x" },
                "b": { "val": "y" },
                "c": { "other": 1 }
            }
        });
        let steps = &[Step::RenameWrapArray("items", "val", "vals")];
        apply_steps(&mut root, steps).unwrap();

        assert_eq!(root["items"]["a"]["vals"], json!(["x"]));
        assert_eq!(root["items"]["b"]["vals"], json!(["y"]));
        // c had no "val" field — unchanged.
        assert!(root["items"]["c"].get("vals").is_none());
        assert_eq!(root["items"]["c"]["other"], 1);
    }
}
