// ===========================================================================
// /api/admin/configure — runtime reconfigure of name / task / models.
//
// Why this exists: Cube takes the cube-template snapshot during the
// dyson-swarm warmup boot, when SWARM_MODEL / SWARM_TASK / etc are
// unset.  On instance create, Cube restores the snapshot — preserving
// the running dyson process's frozen `/proc/self/environ`, so the env
// envelope swarm injects on cube.create_sandbox never reaches the
// agent.  Result without this endpoint: every dyson instance shows
// "warmup-placeholder" as its model and no IDENTITY.md / mission.
//
// dyson-orchestrator's instance.create() POSTs here right after the
// sandbox flips Live with the real env (model list, task, name,
// instance id).  This handler:
//   1. Writes IDENTITY.md to the workspace — picked up by the
//      `HotReloader` on the next agent turn (no process restart).
//   2. Patches dyson.json's `providers.openrouter.models` (or the
//      configured agent provider) — also `HotReloader`-watched, so
//      the next agent build uses the new model list.
//
// Auth: same as every `/api/*` route — `state.auth` validates the
// inbound bearer.  When dyson booted in dangerous-no-auth (warmup),
// any caller is accepted, which is how swarm gets the very first
// configure call through after the snapshot restore (the dyson
// process still thinks it's in warmup mode).  The sandbox is
// network-isolated except via cubeproxy, so "any caller" is in
// practice "swarm via dyson_proxy".

use hyper::Request;
use serde::Deserialize;
use serde_json::Value;

use super::super::responses::{Resp, bad_request, json_ok, read_json_capped, unauthorized};
use super::super::state::HttpState;

/// Cap for the configure body.  Generous for very long task prompts
/// but small enough to swat away accidental large payloads.
const MAX_CONFIGURE_BODY: usize = 64 * 1024;

/// Header swarm sends with the per-instance configure secret
/// (32-hex plaintext from `Uuid::new_v4().simple()`).  Dyson hashes
/// it on first sighting (TOFU) and verifies on every subsequent call.
const CONFIGURE_HEADER: &str = "x-swarm-configure";

/// Filename inside the dyson home dir that holds the argon2id hash
/// of the configure secret.  Lives next to `workspace/`, persists
/// across cube restores (it's in the writable layer).  PHC string
/// format (`$argon2id$v=19$...`) so argon2's verifier can re-derive
/// the salt.
const CONFIGURE_HASH_FILENAME: &str = "configure_secret_hash";

#[derive(Debug, Deserialize)]
pub(super) struct ConfigureBody {
    /// New employee name (e.g. "PR reviewer for foo/bar").
    /// Folded into IDENTITY.md as `Name: <value>`.
    #[serde(default)]
    name: Option<String>,
    /// New mission text.  Folded into IDENTITY.md under a "## Mission"
    /// section so the agent reads it via `Workspace::system_prompt`
    /// on every turn.
    #[serde(default)]
    task: Option<String>,
    /// New ordered model list.  First entry becomes the primary; the
    /// full list is written to `providers.<agent_provider>.models`
    /// in dyson.json so the next `HotReloader::check` picks it up.
    /// Empty list is a no-op (existing config is left alone).
    #[serde(default)]
    models: Vec<String>,
    /// Swarm-side instance id.  Surfaced in IDENTITY.md as
    /// `Swarm instance id: <value>` so the agent can reference it
    /// in tool calls back to swarm.
    #[serde(default)]
    instance_id: Option<String>,
    /// Replacement value for `providers.<agent.provider>.api_key` —
    /// the per-instance proxy_token swarm minted at create time.
    /// Without this, the dyson.json keeps the boot-time
    /// `warmup-placeholder` literal as its api_key (Cube freezes
    /// `/proc/self/environ` at warmup, so the `SWARM_PROXY_TOKEN`
    /// env swarm injects on instance create never reaches the
    /// running dyson process).
    #[serde(default)]
    proxy_token: Option<String>,
    /// Replacement value for `providers.<agent.provider>.base_url` —
    /// swarm's `/llm` URL the agent should call.  Same root cause
    /// as `proxy_token`: the boot-time value is empty / loopback
    /// and Cube's snapshot freeze means swarm can't ride env vars
    /// to fix it.
    #[serde(default)]
    proxy_base: Option<String>,
}

pub(super) async fn post(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    // Pull the configure secret BEFORE consuming the body — the
    // header check runs first so an unauthenticated caller can't
    // make us read a 64 KiB body just to reject it.
    let secret = match req
        .headers()
        .get(CONFIGURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_owned(),
        None => return unauthorized(state),
    };

    // Resolve the hash file's path.  Living next to `workspace/`
    // means a cube template restore picks it up via the writable
    // layer — same spot dyson_home resolves to from
    // `dyson swarm`'s DYSON_HOME env (default /var/lib/dyson).
    let snapshot = state.settings_snapshot();
    let hash_dir = workspace_parent_dir(&snapshot.workspace.connection_string.expose());
    let hash_path = hash_dir.join(CONFIGURE_HASH_FILENAME);

    // TOFU: if no hash on disk, this is the first call — argon2id
    // the inbound plaintext and persist.  Any later call presenting
    // a different plaintext is rejected.  Single-tenant, so the
    // first caller IS swarm (network isolation gates access to
    // cubeproxy in the first place).
    use argon2::password_hash::{PasswordHash, PasswordVerifier, PasswordHasher, SaltString, rand_core::OsRng};
    use argon2::Argon2;
    if let Ok(stored) = std::fs::read_to_string(&hash_path) {
        let parsed = match PasswordHash::new(stored.trim()) {
            Ok(p) => p,
            Err(e) => return bad_request(&format!("stored hash unreadable: {e}")),
        };
        if Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_err()
        {
            return unauthorized(state);
        }
    } else {
        let salt = SaltString::generate(&mut OsRng);
        let hash = match Argon2::default().hash_password(secret.as_bytes(), &salt) {
            Ok(h) => h.to_string(),
            Err(e) => return bad_request(&format!("argon2: {e}")),
        };
        if let Err(e) = std::fs::create_dir_all(&hash_dir) {
            return bad_request(&format!("mkdir {}: {e}", hash_dir.display()));
        }
        if let Err(e) = std::fs::write(&hash_path, hash) {
            return bad_request(&format!("write {}: {e}", hash_path.display()));
        }
        // Mode 0600 on Unix — only the dyson process should read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&hash_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    let body: ConfigureBody = match read_json_capped(req, MAX_CONFIGURE_BODY).await {
        Ok(b) => b,
        Err(e) => return bad_request(&e),
    };

    // 1. Workspace: rewrite IDENTITY.md from the new fields.  Empty
    //    fields are skipped (so a configure carrying only `models`
    //    won't blank the existing identity).
    let identity_changed = if body.name.is_some()
        || body.task.is_some()
        || body.instance_id.is_some()
    {
        let mut ws = match crate::workspace::create_workspace(&snapshot.workspace) {
            Ok(w) => w,
            Err(e) => return bad_request(&format!("workspace open failed: {e}")),
        };
        // Merge: keep the existing IDENTITY.md fields when the new
        // body omits them, so a partial update doesn't wipe identity.
        // Extract first into owned Strings so the merge doesn't dangle
        // references to temporaries.
        let existing = ws.get("IDENTITY.md").unwrap_or_default();
        let prior_name = extract_field(&existing, "Name");
        let prior_instance = extract_field(&existing, "Swarm instance id");
        let prior_mission = extract_section(&existing, "Mission");
        let merged = build_identity_md(
            body.name.as_deref().or(prior_name.as_deref()),
            body.instance_id.as_deref().or(prior_instance.as_deref()),
            body.task.as_deref().or(prior_mission.as_deref()),
        );
        ws.set("IDENTITY.md", &merged);
        if let Err(e) = ws.save() {
            return bad_request(&format!("workspace save failed: {e}"));
        }
        true
    } else {
        false
    };

    // 2. dyson.json: patch the agent provider's `models`, `api_key`,
    //    and/or `base_url` if the body supplies them.  All three
    //    targets share the same patch helper because the surface is
    //    a single `providers.<agent.provider>` object — one
    //    read-modify-write keeps the file in a consistent state and
    //    the HotReloader fires once per change cluster instead of
    //    three times.  Empty / None on a field means "leave alone".
    let want_models   = !body.models.is_empty();
    let want_api_key  = body.proxy_token.as_deref().is_some_and(|s| !s.is_empty());
    let want_base_url = body.proxy_base.as_deref().is_some_and(|s| !s.is_empty());
    let provider_changed = if want_models || want_api_key || want_base_url {
        match state.config_path() {
            Some(path) => match patch_provider_in_config(
                path,
                if want_models { Some(body.models.as_slice()) } else { None },
                if want_api_key { body.proxy_token.as_deref() } else { None },
                if want_base_url { body.proxy_base.as_deref() } else { None },
            ) {
                Ok(()) => true,
                Err(e) => return bad_request(&format!("config patch failed: {e}")),
            },
            None => false,
        }
    } else {
        false
    };
    let models_changed = provider_changed && want_models;

    // Eagerly reload the settings + ClientRegistry instead of waiting
    // for the 2s polling HotReloader to notice the mtime change.  Two
    // reasons this matters:
    //   1. Cube snapshot/restore freezes the dyson process — there's
    //      a real possibility the program-level hot-reload tokio task
    //      doesn't survive the resume cleanly, leaving the registry
    //      pinned to its warmup-time clients (api_key
    //      "warmup-placeholder", base_url api.openai.com).  The chat
    //      then 401s against api.openai.com on every turn.
    //   2. Even when the polling loop IS alive, a chat that fires
    //      between the patch and the next 2s tick caches the warmup
    //      client; the per-chat HotReloader's baseline is then
    //      post-patch, so subsequent turns see no change and never
    //      rebuild.  Eager reload closes the window entirely.
    if provider_changed
        && let Some(path) = state.config_path()
    {
        match crate::config::loader::load_settings(Some(path)) {
            Ok(new_settings) => {
                state.registry.reload(&new_settings, None);
                if let Ok(mut g) = state.settings.write() {
                    *g = new_settings.clone();
                }
                crate::controller::publish_settings(std::sync::Arc::new(new_settings));
                tracing::info!("dyson.json patched + registry reloaded by /api/admin/configure");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "post-patch settings reload failed; falling back to polling HotReloader"
                );
            }
        }
    }

    json_ok(&serde_json::json!({
        "ok": true,
        "identity_updated": identity_changed,
        "models_updated": models_changed,
        "provider_updated": provider_changed,
    }))
}

/// Resolve the directory the configure-secret hash lives in.  We
/// keep it next to the workspace so cube template restores preserve
/// it via the writable layer.  `connection_string` for the in-memory
/// workspace is its directory path; for the file-backed default
/// it's the directory directly.  For unknown shapes we fall back to
/// `/var/lib/dyson` which matches `dyson swarm`'s default home.
fn workspace_parent_dir(connection_string: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(connection_string);
    if let Some(parent) = p.parent().filter(|p| !p.as_os_str().is_empty()) {
        parent.to_path_buf()
    } else {
        std::path::PathBuf::from("/var/lib/dyson")
    }
}

/// Render the IDENTITY.md body in the same shape `dyson swarm` writes
/// at boot.  `Workspace::system_prompt()` injects the file under the
/// `## IDENTITY` section of the agent's system prompt, so the format
/// here is read by the model on every turn.
fn build_identity_md(
    name: Option<&str>,
    instance_id: Option<&str>,
    mission: Option<&str>,
) -> String {
    let mut body = String::from("# Identity\n\n");
    if let Some(n) = name.filter(|s| !s.is_empty()) {
        body.push_str(&format!("Name: {n}\n"));
    }
    if let Some(id) = instance_id.filter(|s| !s.is_empty()) {
        body.push_str(&format!("Swarm instance id: {id}\n"));
    }
    if let Some(m) = mission.filter(|s| !s.is_empty()) {
        body.push_str(&format!("\n## Mission\n\n{m}\n"));
    }
    body
}

/// Crude `Key: value` line scanner — good enough for the two top-of-file
/// fields IDENTITY.md uses.  Returns the trimmed value or None.
fn extract_field(body: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    body.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Pull the body of a `## <name>` section up to the next `##` heading
/// or end of file.  Used to preserve existing mission text when a
/// configure carries only `name` / `instance_id`.
fn extract_section(body: &str, name: &str) -> Option<String> {
    let header = format!("## {name}");
    let mut found = false;
    let mut out = String::new();
    for line in body.lines() {
        if found {
            if line.starts_with("## ") {
                break;
            }
            out.push_str(line);
            out.push('\n');
        } else if line.trim() == header {
            found = true;
        }
    }
    let trimmed = out.trim().to_owned();
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

/// Read dyson.json, patch any of `providers.<agent.provider>.models`,
/// `.api_key`, `.base_url` that the caller supplies, write back
/// atomically.  `None` for a field means "leave alone"; an empty
/// `Some("")` would also be no-op but the caller is expected to filter
/// those out before calling.
///
/// Atomicity matters: `HotReloader` debounces 500ms on mtime so a
/// half-written file isn't a real risk, but rename gives us the
/// belt-and-braces guarantee that a crash mid-write leaves the
/// previous version in place.
fn patch_provider_in_config(
    path: &std::path::Path,
    models: Option<&[String]>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;

    let provider_name = doc
        .get("agent")
        .and_then(|a| a.get("provider"))
        .and_then(|p| p.as_str())
        .ok_or_else(|| "config has no agent.provider — can't tell which provider's config to patch".to_string())?
        .to_owned();

    let providers = doc
        .get_mut("providers")
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| "config has no providers object".to_string())?;
    let prov_entry = providers
        .get_mut(&provider_name)
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| format!("config has no providers.{provider_name}"))?;
    if let Some(ms) = models {
        prov_entry.insert(
            "models".into(),
            Value::Array(ms.iter().map(|m| Value::String(m.clone())).collect()),
        );
    }
    if let Some(k) = api_key {
        prov_entry.insert("api_key".into(), Value::String(k.to_owned()));
    }
    if let Some(u) = base_url {
        prov_entry.insert("base_url".into(), Value::String(u.to_owned()));
    }

    // Atomic write: tmp file in same dir + rename.
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&tmp, &pretty).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_identity_md_skips_empty_sections() {
        let s = build_identity_md(Some("Bob"), Some("u1"), None);
        assert!(s.contains("Name: Bob"));
        assert!(s.contains("Swarm instance id: u1"));
        assert!(!s.contains("## Mission"));
    }

    #[test]
    fn build_identity_md_full() {
        let s = build_identity_md(Some("Bob"), Some("u1"), Some("Watch PRs."));
        assert!(s.contains("## Mission\n\nWatch PRs."));
    }

    #[test]
    fn extract_field_picks_first_match() {
        let b = "Name: Alice\nSwarm instance id: u9\n";
        assert_eq!(extract_field(b, "Name"), Some("Alice".into()));
        assert_eq!(extract_field(b, "Swarm instance id"), Some("u9".into()));
        assert_eq!(extract_field(b, "Missing"), None);
    }

    #[test]
    fn extract_section_keeps_only_named_block() {
        let b = "# Identity\n\nName: A\n\n## Mission\n\nDo the thing.\n\n## Other\n\nelse";
        assert_eq!(
            extract_section(b, "Mission"),
            Some("Do the thing.".into())
        );
        assert_eq!(extract_section(b, "Other"), Some("else".into()));
        assert_eq!(extract_section(b, "Nope"), None);
    }

    #[test]
    fn patch_models_round_trip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": {
                "openrouter": {
                    "type": "openai",
                    "api_key": "warmup-placeholder",
                    "models": ["warmup-placeholder"]
                }
            }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        // `base_url` must NOT carry `/v1` — `OpenAiCompatClient` appends
        // `/v1/chat/completions` itself when building the request URL.  A
        // base ending in `/v1` doubles up to `/openrouter/v1/v1/...`,
        // which routes to OR's marketing site and surfaces as a generic
        // "upstream HTTP error".
        patch_provider_in_config(
            &path,
            Some(&["anthropic/claude-sonnet-4-5".into(), "openai/gpt-5".into()]),
            Some("dy-real-token"),
            Some("https://dyson.example/llm/openrouter"),
        )
        .unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let prov = &after["providers"]["openrouter"];
        assert_eq!(prov["api_key"], "dy-real-token");
        assert_eq!(prov["base_url"], "https://dyson.example/llm/openrouter");
        let models = prov["models"].as_array().unwrap();
        assert_eq!(models[0], "anthropic/claude-sonnet-4-5");
        assert_eq!(models[1], "openai/gpt-5");
    }
}
