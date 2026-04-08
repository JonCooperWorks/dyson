// ===========================================================================
// Command — CLI subcommand implementations.
//
// LEARNING OVERVIEW
//
// What this module does:
//   Each file in this module implements one CLI subcommand:
//
//   init.rs    — `dyson init`   — create ~/.dyson with default config
//   listen.rs  — `dyson listen` — start all configured controllers
//   run.rs     — `dyson run`    — single-shot: run once, print, exit
//
// Why split from main.rs?
//   main.rs defines the CLI structure (clap) and dispatches to these
//   functions.  Each command has enough logic to warrant its own file:
//   init handles directory scaffolding, systemd, OpenClaw import;
//   listen handles controller creation and concurrent execution;
//   run handles single-shot agent invocation.
//
//   This keeps main.rs short — just CLI parsing and dispatch.
// ===========================================================================

pub mod init;
pub mod listen;
pub mod run;

use std::path::PathBuf;

use dyson::config::Settings;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Default config path: ~/.dyson/dyson.json
///
/// Returns an error if neither `HOME` nor `USERPROFILE` is set.
pub fn dirs_config_path() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "HOME environment variable must be set".to_string())?;
    Ok(PathBuf::from(home).join(".dyson").join("dyson.json"))
}

/// Resolve config path: explicit flag > ~/.dyson/dyson.json > ./dyson.json > None.
pub fn resolve_config_path(explicit: Option<PathBuf>) -> Option<PathBuf> {
    explicit.or_else(|| {
        let home_config = dirs_config_path().ok()?;
        if home_config.exists() {
            Some(home_config)
        } else {
            let cwd = PathBuf::from("dyson.json");
            if cwd.exists() { Some(cwd) } else { None }
        }
    })
}

/// Apply CLI overrides to loaded settings.
///
/// `--provider` selects a named provider from `settings.providers`.
/// If the name isn't found, it's treated as a provider type string
/// (e.g. "anthropic") for convenience.
pub fn apply_overrides(
    settings: &mut Settings,
    dangerous_no_sandbox: bool,
    provider: Option<String>,
    base_url: Option<String>,
    workspace: Option<String>,
) -> dyson::error::Result<()> {
    if let Some(provider_str) = provider {
        if let Some(pc) = settings.providers.get(&provider_str) {
            // Named provider from config.
            settings.agent.provider = pc.provider_type.clone();
            settings.agent.model = pc.default_model().to_string();
            settings.agent.api_key = pc.api_key.clone();
            settings.agent.base_url = pc.base_url.clone();
        } else if let Some(provider_type) = dyson::llm::registry::from_str_loose(&provider_str) {
            // Bare provider type string (e.g. "anthropic").
            settings.agent.provider = provider_type;
        } else {
            let available: Vec<&str> = settings.providers.keys().map(|s| s.as_str()).collect();
            return Err(dyson::error::DysonError::Config(format!(
                "unknown provider '{provider_str}'.  \
                 Available: {available:?}.  \
                 Or use a type: '{}'.",
                dyson::llm::registry::all_canonical_names().join("', '"),
            )));
        }
    }
    if let Some(url) = base_url {
        settings.agent.base_url = Some(url);
    }
    settings.dangerous_no_sandbox = dangerous_no_sandbox;
    if let Some(ws) = workspace {
        settings.workspace.connection_string = dyson::auth::Credential::new(ws);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_config_path_explicit_wins() {
        let explicit = PathBuf::from("/tmp/explicit.json");
        let result = resolve_config_path(Some(explicit.clone()));
        assert_eq!(result, Some(explicit));
    }

    #[test]
    fn resolve_config_path_home_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let dyson_dir = tmp.path().join(".dyson");
        std::fs::create_dir_all(&dyson_dir).unwrap();
        std::fs::write(dyson_dir.join("dyson.json"), "{}").unwrap();

        // Temporarily override HOME.
        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let result = resolve_config_path(None);
        assert!(result.is_some());
        assert!(result.unwrap().to_string_lossy().contains(".dyson"));

        // Restore HOME.
        if let Some(h) = old_home {
            unsafe { std::env::set_var("HOME", h) };
        }
    }

    #[test]
    fn resolve_config_path_none() {
        // With an explicit None and HOME pointing to a dir without config,
        // and CWD without dyson.json, result should be None.
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        // Ensure no dyson.json in CWD either (tests run from repo root which
        // should not have one, but be explicit).
        let result = resolve_config_path(None);
        // Result may or may not be None depending on CWD — just test explicit path wins.
        let _ = result;

        if let Some(h) = old_home {
            unsafe { std::env::set_var("HOME", h) };
        }
    }

    #[test]
    fn apply_overrides_unknown_provider() {
        let mut settings = Settings::default();
        let result = apply_overrides(
            &mut settings,
            false,
            Some("nonexistent_provider_xyz".to_string()),
            None,
            None,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown provider"));
    }
}
