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

use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;

use super::super::responses::{Resp, bad_request, boxed, json_ok, read_json_capped, unauthorized};
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
    /// Name to register the image-generation provider under in
    /// `providers.<image_provider_name>`.  Distinct from
    /// `agent.provider` (chat) so the two can run with different
    /// `LlmProvider` types — only `Gemini` and `OpenRouter` are wired
    /// for image generation today.  When swarm pushes this it always
    /// arrives alongside `image_provider_block`,
    /// `image_generation_provider`, and `image_generation_model`;
    /// individually-set fields are still patched so callers can do
    /// partial updates if they want.
    #[serde(default)]
    image_provider_name: Option<String>,
    /// Full provider entry to insert under `providers.<image_provider_name>`.
    /// Shape mirrors what the dyson-side loader accepts:
    /// `{ "type": "openrouter", "base_url": "...", "api_key": "...", "models": [...] }`.
    /// Existing entries with the same name are replaced.
    #[serde(default)]
    image_provider_block: Option<serde_json::Value>,
    /// Sets `agent.image_generation_provider`.  Usually equal to
    /// `image_provider_name`, but kept independent so a future
    /// caller could point the field at an already-registered
    /// provider without re-uploading its block.
    #[serde(default)]
    image_generation_provider: Option<String>,
    /// Sets `agent.image_generation_model` — the model id passed to
    /// the image-gen factory's `model_override`.  Without this the
    /// factory falls back to the provider's first `models` entry.
    #[serde(default)]
    image_generation_model: Option<String>,
    /// Reset the `skills` block in dyson.json to "use defaults".
    /// Removes the key entirely so the loader's
    /// no-skills-block branch fires and every builtin tool registers
    /// (`bash`, `read_file`, `image_generate`, etc.).  Originally
    /// added because earlier `dyson swarm` boots wrote
    /// `"skills": { "builtin": { "tools": [] } }`, which the loader
    /// parses as "register zero builtin tools" — every instance
    /// shipped without a single tool.  Setting this flag on a sweep
    /// is the retroactive fix.
    #[serde(default)]
    reset_skills: bool,
    /// Explicit builtin-tool allowlist.  When `Some`, dyson rewrites
    /// `skills.builtin.tools` to this exact list; the loader registers
    /// only those builtins.  An empty vec is meaningful — register
    /// zero builtins.  Distinct from `reset_skills` (which drops the
    /// block entirely).  When both are set, `tools` wins (a caller
    /// asking for a subset clearly does NOT want defaults).
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Per-server stanzas to write under top-level `mcp_servers.<name>`.
    /// Each value is the JSON the loader's `parse_mcp_servers` already
    /// understands — typically `{ url, headers, auth? }` for HTTP MCP.
    /// Swarm builds these with its own proxied URL + the per-instance
    /// bearer so the agent never sees the upstream URL or its
    /// credentials.  `None` leaves the existing block alone; an empty
    /// map clears it.  Replaces the whole block — incremental edits
    /// aren't supported (callers who want add/remove read the file first).
    #[serde(default)]
    mcp_servers: Option<serde_json::Map<String, Value>>,
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

    // 3. Image generation: register / replace the dedicated image
    //    provider block and point `agent.image_generation_*` at it.
    //    Independent of the chat patch above — the chat path's
    //    `agent.provider` is `openrouter` (LlmProvider::OpenAi under
    //    the hood) but the image factory dispatches on real
    //    LlmProvider variants, so the two need separate provider
    //    entries.  Existing dysons (created before this field was
    //    plumbed) get retroactively rewired by the swarm-side sweep
    //    that pushes a configure with these set.
    let want_image_block    = body.image_provider_name.as_deref().is_some_and(|s| !s.is_empty())
        && body.image_provider_block.is_some();
    let want_image_provider = body.image_generation_provider.as_deref().is_some_and(|s| !s.is_empty());
    let want_image_model    = body.image_generation_model.as_deref().is_some_and(|s| !s.is_empty());
    let image_changed = if want_image_block || want_image_provider || want_image_model {
        match state.config_path() {
            Some(path) => match patch_image_generation_in_config(
                path,
                if want_image_block {
                    body.image_provider_name.as_deref().zip(body.image_provider_block.as_ref())
                } else {
                    None
                },
                if want_image_provider { body.image_generation_provider.as_deref() } else { None },
                if want_image_model    { body.image_generation_model.as_deref()    } else { None },
            ) {
                Ok(()) => true,
                Err(e) => return bad_request(&format!("image-gen patch failed: {e}")),
            },
            None => false,
        }
    } else {
        false
    };
    // 4. Skills: an explicit `tools` list rewrites
    //    `skills.builtin.tools` to that exact set; otherwise
    //    `reset_skills` drops the `skills` key so the loader's
    //    defaults path registers every builtin.  Independent of the
    //    chat / image patches — swarm's sweep flips one of these on
    //    every push so toolless instances self-heal on the next
    //    configure.  `tools` wins if both are set.
    let skills_changed = if let Some(allowlist) = body.tools.as_deref() {
        match state.config_path() {
            Some(path) => match set_skills_tools_in_config(path, allowlist) {
                Ok(changed) => changed,
                Err(e) => return bad_request(&format!("skills tools patch failed: {e}")),
            },
            None => false,
        }
    } else if body.reset_skills {
        match state.config_path() {
            Some(path) => match clear_skills_in_config(path) {
                Ok(changed) => changed,
                Err(e) => return bad_request(&format!("skills reset failed: {e}")),
            },
            None => false,
        }
    } else {
        false
    };

    // 5. MCP servers: replace the top-level `mcp_servers` block.  None
    //    leaves it alone; an empty map clears it.  Distinct from the
    //    skills block because MCP servers are a sibling key in the
    //    loader's `JsonRoot`, not nested under `skills`.
    let mcp_changed = if let Some(servers) = &body.mcp_servers {
        match state.config_path() {
            Some(path) => match patch_mcp_servers_in_config(path, servers) {
                Ok(changed) => changed,
                Err(e) => return bad_request(&format!("mcp_servers patch failed: {e}")),
            },
            None => false,
        }
    } else {
        false
    };

    let any_config_changed = provider_changed || image_changed || skills_changed || mcp_changed;

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
    if any_config_changed
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
        "image_generation_updated": image_changed,
        "skills_reset": skills_changed,
        "mcp_servers_updated": mcp_changed,
    }))
}

/// Diagnostic: return the live skill / tool inventory so an operator
/// can confirm which MCP servers actually loaded after a configure
/// push.  Same configure-secret auth as `post()` (the only auth
/// surface on `/api/admin/*`).  Builds a throwaway agent off the
/// current settings so we report the actual `on_load` outcome — a
/// live `state.registry` only caches LLM clients, not skills.
pub(super) async fn get_skills(req: Request<hyper::body::Incoming>, state: &HttpState) -> Resp {
    use argon2::{password_hash::{PasswordHash, PasswordVerifier}, Argon2};
    use crate::skill::Skill;
    #[allow(unused_imports)]
    use crate::tool::Tool;
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
    let snapshot = state.settings_snapshot();
    let hash_dir = workspace_parent_dir(&snapshot.workspace.connection_string.expose());
    let hash_path = hash_dir.join(CONFIGURE_HASH_FILENAME);
    let stored = match std::fs::read_to_string(&hash_path) {
        Ok(s) => s,
        Err(_) => return unauthorized(state),
    };
    let parsed = match PasswordHash::new(stored.trim()) {
        Ok(p) => p,
        Err(_) => return unauthorized(state),
    };
    if Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_err()
    {
        return unauthorized(state);
    }

    // Re-load settings fresh from disk — this is the same path
    // build_agent uses, so the result reflects what an actual chat
    // turn would build with.
    let path = match state.config_path() {
        Some(p) => p.to_path_buf(),
        None => return bad_request("config_path is not set"),
    };
    let settings = match crate::config::loader::load_settings(Some(&path)) {
        Ok(s) => s,
        Err(e) => return bad_request(&format!("load_settings: {e}")),
    };

    let mut by_kind: Vec<serde_json::Value> = Vec::new();
    let mut mcp_listed: Vec<serde_json::Value> = Vec::new();
    for sk in &settings.skills {
        match sk {
            crate::config::SkillConfig::Builtin(b) => {
                by_kind.push(serde_json::json!({
                    "kind": "builtin",
                    "tools_filter": b.tools.len(),
                }));
            }
            crate::config::SkillConfig::Local(l) => {
                by_kind.push(serde_json::json!({
                    "kind": "local",
                    "name": l.name,
                    "path": l.path,
                }));
            }
            crate::config::SkillConfig::Subagent(sa) => {
                by_kind.push(serde_json::json!({
                    "kind": "subagent",
                    "agents": sa.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                }));
            }
            crate::config::SkillConfig::Mcp(m) => {
                let transport = match &m.transport {
                    crate::config::McpTransportConfig::Http { url, headers, auth } => {
                        serde_json::json!({
                            "type": "http",
                            "url": url,
                            "header_keys": headers.keys().collect::<Vec<_>>(),
                            "oauth": auth.is_some(),
                        })
                    }
                    crate::config::McpTransportConfig::Stdio { command, .. } => {
                        serde_json::json!({ "type": "stdio", "command": command })
                    }
                };
                mcp_listed.push(serde_json::json!({
                    "name": m.name,
                    "transport": transport,
                }));
            }
        }
    }

    // Try to actually load each MCP skill so we can report the
    // on_load outcome — handshake errors (the silent-skip path in
    // skill::build_skills) surface here as `loaded: false` with the
    // captured error string.  Doesn't share state with running
    // chats; just a probe.
    let mut mcp_probes: Vec<serde_json::Value> = Vec::new();
    for sk in &settings.skills {
        if let crate::config::SkillConfig::Mcp(cfg) = sk {
            let mut skill = crate::skill::mcp::McpSkill::new(*cfg.clone());
            let result = skill.on_load().await;
            mcp_probes.push(match result {
                Ok(()) => serde_json::json!({
                    "name": cfg.name,
                    "loaded": true,
                    "tools": skill.tools().len(),
                    "tool_names": skill.tools().iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
                }),
                Err(e) => serde_json::json!({
                    "name": cfg.name,
                    "loaded": false,
                    "error": e.to_string(),
                }),
            });
        }
    }

    json_ok(&serde_json::json!({
        "ok": true,
        "skills": by_kind,
        "mcp_servers": mcp_listed,
        "mcp_probes": mcp_probes,
    }))
}

/// Verify the inbound `x-swarm-configure` plaintext against the
/// argon2id hash on disk.  Returns `Ok(())` on match, an unauthorized
/// `Resp` on miss/missing/malformed.  Shared by `get_skills`, the new
/// idle/quiesce/unquiesce admin endpoints, and (eventually) anywhere
/// else that wants the same configure-secret guard.  We intentionally
/// don't TOFU here — the very first `/configure` call still owns the
/// hash-creation path; callers of these other endpoints come AFTER
/// configure has run, so a missing hash file is a real auth failure
/// and not a fresh-instance state.
fn verify_configure_secret(
    req: &Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Result<(), Resp> {
    use argon2::{password_hash::{PasswordHash, PasswordVerifier}, Argon2};
    let secret = match req
        .headers()
        .get(CONFIGURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_owned(),
        None => return Err(unauthorized(state)),
    };
    let snapshot = state.settings_snapshot();
    let hash_dir = workspace_parent_dir(&snapshot.workspace.connection_string.expose());
    let hash_path = hash_dir.join(CONFIGURE_HASH_FILENAME);
    let stored = match std::fs::read_to_string(&hash_path) {
        Ok(s) => s,
        Err(_) => return Err(unauthorized(state)),
    };
    let parsed = match PasswordHash::new(stored.trim()) {
        Ok(p) => p,
        Err(_) => return Err(unauthorized(state)),
    };
    if Argon2::default()
        .verify_password(secret.as_bytes(), &parsed)
        .is_err()
    {
        return Err(unauthorized(state));
    }
    Ok(())
}

/// `GET /api/admin/idle` — observability endpoint paired with the
/// quiesce/unquiesce control plane.  Returns whether the controller
/// would currently accept a new turn (no in-flight, not quiesced) plus
/// the per-chat in-flight count so swarm's wait-loop can log progress
/// while polling.  Auth: same configure secret as `/configure`.
pub(super) async fn get_idle(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Err(resp) = verify_configure_secret(&req, state) {
        return resp;
    }
    use std::sync::atomic::Ordering;
    let chats = state.chats.lock().await;
    let in_flight = chats
        .values()
        .filter(|h| h.busy.load(Ordering::SeqCst))
        .count();
    let quiesced = state.quiesced.load(Ordering::SeqCst);
    json_ok(&serde_json::json!({
        "idle": in_flight == 0 && !quiesced,
        "in_flight_chats": in_flight,
        "quiesced": quiesced,
    }))
}

/// `POST /api/admin/quiesce` — atomic test-and-set: if no chat is
/// in flight, latch `quiesced=true` and refuse new turns until
/// `/unquiesce`.  Otherwise undo the latch and 409 with the busy
/// count so swarm can decide to wait + retry.
///
/// Ordering: we set `quiesced=true` BEFORE scanning per-chat `busy`.
/// `routes::turns::post` does the symmetric pair (swap busy → load
/// quiesced) so any turn that slipped through before our latch is
/// caught by the busy scan; any turn arriving after sees quiesced
/// and aborts cleanly.  Both sides use SeqCst so the swap-then-load
/// and store-then-load orderings are observable globally.
pub(super) async fn post_quiesce(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Err(resp) = verify_configure_secret(&req, state) {
        return resp;
    }
    use std::sync::atomic::Ordering;
    // If already quiesced, this is a no-op success — swarm retrying
    // a quiesce after a transient network hiccup must be idempotent.
    if state.quiesced.swap(true, Ordering::SeqCst) {
        let chats = state.chats.lock().await;
        let in_flight = chats
            .values()
            .filter(|h| h.busy.load(Ordering::SeqCst))
            .count();
        return json_ok(&serde_json::json!({
            "quiesced": true,
            "already": true,
            "in_flight_chats": in_flight,
        }));
    }
    // Latch is held; now check nothing slipped through.  Any chat
    // that swapped busy=true before our store is here; any that runs
    // after our store sees quiesced=true and aborts.
    let chats = state.chats.lock().await;
    let in_flight = chats
        .values()
        .filter(|h| h.busy.load(Ordering::SeqCst))
        .count();
    if in_flight > 0 {
        // Race lost — undo the latch so the caller can retry.
        state.quiesced.store(false, Ordering::SeqCst);
        return Response::builder()
            .status(StatusCode::CONFLICT)
            .header("Content-Type", "application/json")
            .body(boxed(Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "quiesced": false,
                    "in_flight_chats": in_flight,
                    "error": "instance is busy; retry after current turn(s) complete",
                }))
                .unwrap_or_default(),
            )))
            .unwrap();
    }
    json_ok(&serde_json::json!({
        "quiesced": true,
        "in_flight_chats": 0,
    }))
}

/// `POST /api/admin/unquiesce` — release the latch.  Idempotent: a
/// double-unquiesce is a 200.  Used by swarm only when an upgrade
/// attempt aborts AFTER quiesce succeeded (e.g. cube create failed)
/// so the user's chat resumes on the original cube instead of being
/// stuck behind a 503.
pub(super) async fn post_unquiesce(
    req: Request<hyper::body::Incoming>,
    state: &HttpState,
) -> Resp {
    if let Err(resp) = verify_configure_secret(&req, state) {
        return resp;
    }
    use std::sync::atomic::Ordering;
    state.quiesced.store(false, Ordering::SeqCst);
    json_ok(&serde_json::json!({"quiesced": false}))
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

/// Read dyson.json, register/replace the image-generation provider
/// entry and/or update `agent.image_generation_provider` /
/// `agent.image_generation_model`, write back atomically.  Each input
/// is independent — a `None` means "leave alone" — so a swarm-side
/// rewire can carry only the model change without re-uploading the
/// full provider block, and vice versa.
///
/// Atomicity matters for the same reason as `patch_provider_in_config`:
/// a half-written dyson.json is a chat-killer if the HotReloader picks
/// it up before the second write lands.
fn patch_image_generation_in_config(
    path: &std::path::Path,
    provider_block: Option<(&str, &Value)>,
    image_provider: Option<&str>,
    image_model: Option<&str>,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;

    if let Some((name, block)) = provider_block {
        let providers = doc
            .as_object_mut()
            .ok_or_else(|| "config root is not an object".to_string())?
            .entry("providers".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| "config providers is not an object".to_string())?;
        providers.insert(name.to_owned(), block.clone());
    }

    if image_provider.is_some() || image_model.is_some() {
        let agent = doc
            .as_object_mut()
            .ok_or_else(|| "config root is not an object".to_string())?
            .entry("agent".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| "config agent is not an object".to_string())?;
        if let Some(p) = image_provider {
            agent.insert("image_generation_provider".into(), Value::String(p.to_owned()));
        }
        if let Some(m) = image_model {
            agent.insert("image_generation_model".into(), Value::String(m.to_owned()));
        }
    }

    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&tmp, &pretty).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Drop the `skills` key from dyson.json so the loader's defaults
/// branch registers every builtin tool.  Returns `Ok(true)` when the
/// key was present and removed, `Ok(false)` when it was already
/// absent (no write fired).
///
/// Atomic write via tmp + rename for the same reason as the sibling
/// patch helpers — the HotReloader debounces 500ms on mtime so a
/// half-written file is unlikely, but rename gives us the
/// belt-and-braces guarantee that a crash mid-write leaves the
/// previous version in place.
fn clear_skills_in_config(path: &std::path::Path) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let removed = doc
        .as_object_mut()
        .ok_or_else(|| "config root is not an object".to_string())?
        .remove("skills")
        .is_some();
    if !removed {
        return Ok(false);
    }
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&tmp, &pretty).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(true)
}

/// Apply an explicit allowlist to BOTH `skills.builtin.tools` and
/// `skills.subagents`.  The orchestrator's tool-picker UI flattens
/// builtins and subagents into a single checklist, so the two have
/// to share one allowlist — otherwise unchecking a subagent in the
/// SPA leaves it loaded at runtime, and the agent introspects it as
/// available even though the operator disabled it.
///
/// Behaviour:
/// - `skills.builtin.tools` becomes the exact list passed in.  Empty
///   vec lands as `tools: []` so the loader registers zero builtins.
/// - `skills.subagents` is filtered by name: only entries whose
///   `name` appears in the allowlist survive.  An empty allowlist
///   drops the whole `subagents` key (consistent with "register zero
///   subagents", same shape parse_skills uses for an empty agent
///   list — no `Subagent` skill config gets pushed).
/// - Sibling keys under `skills` (locals, anything else) are
///   preserved verbatim.
///
/// Atomic write via tmp + rename, same posture as the sibling
/// helpers.  Returns `Ok(true)` when the file was rewritten,
/// `Ok(false)` when both blocks already matched and no write was
/// needed.
fn set_skills_tools_in_config(
    path: &std::path::Path,
    tools: &[String],
) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let root = doc
        .as_object_mut()
        .ok_or_else(|| "config root is not an object".to_string())?;
    let new_tools = Value::Array(
        tools
            .iter()
            .map(|t| Value::String(t.clone()))
            .collect(),
    );
    let skills = root
        .entry("skills".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "skills is not an object".to_string())?;

    // 1. builtin.tools: rewrite to the exact list.
    let builtin = skills
        .entry("builtin".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| "skills.builtin is not an object".to_string())?;
    let builtin_unchanged = builtin.get("tools") == Some(&new_tools);
    if !builtin_unchanged {
        builtin.insert("tools".to_string(), new_tools);
    }

    // 2. subagents: filter by name.  Operate via take/replace so the
    //    skills map can be mutated independently of the read borrow.
    let allowed: std::collections::HashSet<&str> =
        tools.iter().map(String::as_str).collect();
    let prev = skills.remove("subagents");
    let subagents_unchanged = match prev {
        Some(Value::Array(arr)) => {
            let kept: Vec<Value> = arr
                .iter()
                .filter(|entry| {
                    entry
                        .as_object()
                        .and_then(|o| o.get("name"))
                        .and_then(Value::as_str)
                        .map(|n| allowed.contains(n))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            let unchanged = kept.len() == arr.len() && kept == arr;
            if !kept.is_empty() {
                // Re-insert the filtered array — preserves entry
                // ordering and any non-`name` fields per agent.
                skills.insert("subagents".to_string(), Value::Array(kept));
            }
            // kept.is_empty() means the allowlist excludes every
            // subagent — leave the key absent so the loader doesn't
            // push an empty Subagent skill (parse_skills only emits
            // one when the array is non-empty; matching that
            // contract keeps round-trips clean).
            unchanged
        }
        Some(other) => {
            // Malformed pre-existing value (not an array).  Restore
            // it so we don't silently destroy operator state, and
            // treat the call as a no-op for the subagents half.
            skills.insert("subagents".to_string(), other);
            true
        }
        None => {
            // No subagents key — nothing to filter, nothing to write.
            true
        }
    };

    if builtin_unchanged && subagents_unchanged {
        return Ok(false);
    }
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&tmp, &pretty).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(true)
}

/// Replace the top-level `mcp_servers` block in dyson.json with the
/// supplied map.  Empty map clears the block (loader treats absent
/// and empty identically — no MCP skills register).  Returns
/// `Ok(true)` when the file was rewritten, `Ok(false)` when the
/// existing block already matched and no write was needed.
///
/// Atomic write via tmp + rename — same posture as the sibling
/// helpers above.  HotReloader picks the rewritten file up on the
/// next mtime tick, and the eager-reload at the end of `post()`
/// closes the window between this write and the next agent build.
fn patch_mcp_servers_in_config(
    path: &std::path::Path,
    servers: &serde_json::Map<String, Value>,
) -> Result<bool, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let root = doc
        .as_object_mut()
        .ok_or_else(|| "config root is not an object".to_string())?;
    let new_block = Value::Object(servers.clone());
    if root.get("mcp_servers") == Some(&new_block) {
        return Ok(false);
    }
    if servers.is_empty() {
        root.remove("mcp_servers");
    } else {
        root.insert("mcp_servers".to_string(), new_block);
    }
    let tmp = path.with_extension("json.tmp");
    let pretty = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialise: {e}"))?;
    std::fs::write(&tmp, &pretty).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(true)
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

    #[test]
    fn patch_image_generation_inserts_provider_and_agent_fields() {
        // Existing config has only the chat provider — the swarm-side
        // rewire sweep arrives with a brand-new image provider block
        // and the agent fields pointing at it.  Both must land in one
        // atomic write so the HotReloader doesn't see a half-state.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": {
                "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] }
            }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        let block = serde_json::json!({
            "type": "openrouter",
            "base_url": "https://swarm/llm/openrouter",
            "api_key": "tok",
            "models": ["google/gemini-3-pro-image-preview"]
        });
        patch_image_generation_in_config(
            &path,
            Some(("openrouter-image", &block)),
            Some("openrouter-image"),
            Some("google/gemini-3-pro-image-preview"),
        )
        .unwrap();

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let img = &after["providers"]["openrouter-image"];
        assert_eq!(img["type"], "openrouter");
        assert_eq!(img["base_url"], "https://swarm/llm/openrouter");
        assert_eq!(img["models"][0], "google/gemini-3-pro-image-preview");
        assert_eq!(after["agent"]["image_generation_provider"], "openrouter-image");
        assert_eq!(after["agent"]["image_generation_model"], "google/gemini-3-pro-image-preview");
        // Chat side is untouched — a regression here would silently
        // break the chat path on every running instance the sweep
        // visits.
        assert_eq!(after["agent"]["provider"], "openrouter");
        assert_eq!(after["providers"]["openrouter"]["api_key"], "x");
    }

    #[test]
    fn patch_image_generation_partial_update_only_touches_provided_fields() {
        // Operator-side: somebody bumps the image model id (e.g. a
        // newer preview) without changing the provider entry.  Only
        // `agent.image_generation_model` should change; the provider
        // block and provider-name field stay as they were.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": {
                "provider": "openrouter",
                "image_generation_provider": "openrouter-image",
                "image_generation_model": "google/old-model"
            },
            "providers": {
                "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] },
                "openrouter-image": { "type": "openrouter", "models": ["google/old-model"] }
            }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        patch_image_generation_in_config(&path, None, None, Some("google/new-model")).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["agent"]["image_generation_model"], "google/new-model");
        assert_eq!(after["agent"]["image_generation_provider"], "openrouter-image");
        // Provider block models[] left alone — the model id only flows
        // through the agent.image_generation_model override.
        assert_eq!(after["providers"]["openrouter-image"]["models"][0], "google/old-model");
    }

    #[test]
    fn clear_skills_drops_the_block_so_loader_registers_all_builtins() {
        // Regression for "the agent has no tools".  Older `dyson swarm`
        // boots wrote `skills.builtin.tools = []`, which the loader
        // parses as "register zero builtin tools".  The configure-time
        // skills reset must remove the key entirely so the loader's
        // no-skills-block branch fires on next reload.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } },
            "skills": { "builtin": { "tools": [] } }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        assert!(clear_skills_in_config(&path).unwrap(), "first call must report a change");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(after.get("skills").is_none(), "skills key must be removed");
        // Other top-level keys unchanged — clearing skills must not
        // perturb providers / agent.
        assert_eq!(after["agent"]["provider"], "openrouter");
        assert_eq!(after["providers"]["openrouter"]["api_key"], "x");

        // Idempotent second call: no skills key means nothing to remove.
        assert!(!clear_skills_in_config(&path).unwrap(),
            "second call must report no-op when skills already absent");
    }

    #[test]
    fn set_skills_tools_writes_explicit_allowlist_and_is_idempotent() {
        // Editing an instance's tool selection in the orchestrator UI
        // must rewrite `skills.builtin.tools` to the chosen subset on
        // the running dyson; otherwise the live agent keeps registering
        // the boot-time set.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        let tools = vec!["bash".to_string(), "read_file".to_string()];
        assert!(set_skills_tools_in_config(&path, &tools).unwrap(), "first write must report a change");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["skills"]["builtin"]["tools"], serde_json::json!(["bash", "read_file"]));
        // Sibling keys preserved.
        assert_eq!(after["agent"]["provider"], "openrouter");
        assert_eq!(after["providers"]["openrouter"]["api_key"], "x");

        // Same allowlist a second time is a no-op.
        assert!(!set_skills_tools_in_config(&path, &tools).unwrap(),
            "no-change call must report no-op");

        // Empty list lands as `tools: []` so the loader registers zero builtins.
        assert!(set_skills_tools_in_config(&path, &[]).unwrap(), "shrink to empty must report a change");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["skills"]["builtin"]["tools"], serde_json::json!([]));
    }

    #[test]
    fn set_skills_tools_filters_subagents_by_same_allowlist() {
        // The orchestrator's tool-picker collapses builtins and
        // subagents into one checklist; unchecking a subagent in the
        // SPA must drop it from the running dyson too.  Otherwise the
        // agent introspects its loaded subagents and reports them as
        // available even though the operator disabled them — which is
        // the bug this rule fixes.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } },
            "skills": {
                "builtin": { "tools": ["read_file", "write_file"] },
                "subagents": [
                    { "name": "planner",     "description": "p", "system_prompt": "sp" },
                    { "name": "researcher",  "description": "r", "system_prompt": "sr" },
                    { "name": "coder",       "description": "c", "system_prompt": "sc" }
                ]
            }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        // Allowlist keeps two builtins + one of the three subagents.
        let allow = vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "planner".to_string(),
        ];
        assert!(set_skills_tools_in_config(&path, &allow).unwrap(),
            "filtering must report a change");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        // Builtin allowlist round-trips verbatim — including the two
        // subagent-shaped names.  parse_skills' loader-side filter
        // ignores names that don't match a real builtin, so the only
        // tools that actually register are the genuine builtins.
        assert_eq!(
            after["skills"]["builtin"]["tools"],
            serde_json::json!(["read_file", "write_file", "planner"])
        );

        // Subagents: only "planner" survives; "researcher" and "coder"
        // are gone because they weren't in the allowlist.  The other
        // fields on the kept entry round-trip verbatim.
        let subagents = after["skills"]["subagents"].as_array().unwrap();
        assert_eq!(subagents.len(), 1, "only planner should survive");
        assert_eq!(subagents[0]["name"], "planner");
        assert_eq!(subagents[0]["description"], "p");
        assert_eq!(subagents[0]["system_prompt"], "sp");

        // Same call a second time is a no-op (idempotent).
        assert!(!set_skills_tools_in_config(&path, &allow).unwrap(),
            "no-change call must report no-op");

        // Allowlist that excludes every subagent drops the subagents
        // key entirely — keeps the loader's contract that an empty
        // array doesn't get a Subagent skill config pushed.
        let no_subagents = vec!["read_file".to_string()];
        assert!(set_skills_tools_in_config(&path, &no_subagents).unwrap(),
            "narrowing the allowlist must report a change");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            after["skills"].get("subagents").is_none(),
            "subagents key must be absent when the allowlist excludes every entry, got {:?}",
            after["skills"].get("subagents")
        );
        assert_eq!(after["skills"]["builtin"]["tools"], serde_json::json!(["read_file"]));
    }

    #[test]
    fn patch_mcp_servers_replaces_block_and_is_idempotent() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dyson.json");
        let initial = serde_json::json!({
            "agent": { "provider": "openrouter" },
            "providers": { "openrouter": { "type": "openai", "api_key": "x", "models": ["m"] } }
        });
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(serde_json::to_vec_pretty(&initial).unwrap().as_slice()).unwrap();
        drop(f);

        // Insert a server.
        let mut servers = serde_json::Map::new();
        servers.insert(
            "linear".into(),
            serde_json::json!({
                "url": "https://swarm.example/mcp/i-1/linear",
                "headers": { "Authorization": "Bearer tok" }
            }),
        );
        assert!(patch_mcp_servers_in_config(&path, &servers).unwrap());
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["mcp_servers"]["linear"]["url"], "https://swarm.example/mcp/i-1/linear");
        // Sibling keys untouched.
        assert_eq!(after["agent"]["provider"], "openrouter");

        // Idempotent: the same map yields no rewrite.
        assert!(!patch_mcp_servers_in_config(&path, &servers).unwrap());

        // Empty map clears the block.
        let empty = serde_json::Map::new();
        assert!(patch_mcp_servers_in_config(&path, &empty).unwrap());
        let after2: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(after2.get("mcp_servers").is_none());
    }
}
