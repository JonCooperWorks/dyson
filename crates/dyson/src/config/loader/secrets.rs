//! Resolve secret references and validate credential destinations.
use crate::config::Settings;
use crate::error::{DysonError, Result};
use crate::secret::{SecretRegistry, SecretValue};

/// Walk a JSON value and resolve any secret references in-place.
///
/// A secret reference is a JSON object with exactly `"resolver"` and
/// `"name"` keys.  When found, it's replaced with the resolved string
/// value.  This lets controllers receive fully-resolved config without
/// knowing about the secret system.
///
/// ```json
/// // Before:
/// { "bot_token": { "resolver": "insecure_env", "name": "MY_TOKEN" } }
///
/// // After (if MY_TOKEN=abc123):
/// { "bot_token": "abc123" }
/// ```
pub(super) fn resolve_secrets_in_value(value: &mut serde_json::Value, secrets: &SecretRegistry) {
    match value {
        serde_json::Value::Object(map) => {
            // Check if THIS object is a secret reference.
            if map.len() == 2 && map.contains_key("resolver") && map.contains_key("name") {
                let secret_val = SecretValue::Reference {
                    resolver: map["resolver"].as_str().unwrap_or("").to_string(),
                    name: map["name"].as_str().unwrap_or("").to_string(),
                };
                match secrets.resolve(&secret_val) {
                    Ok(resolved) => {
                        *value = serde_json::Value::String(resolved.expose().to_string());
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "secret resolution failed in controller config");
                        return;
                    }
                }
            }

            // Otherwise, recurse into child values.
            for value in map.values_mut() {
                resolve_secrets_in_value(value, secrets);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                resolve_secrets_in_value(item, secrets);
            }
        }
        // Strings, numbers, bools, null — no resolution needed.
        _ => {}
    }
}

/// Resolve API keys for all providers and the active agent.
///
/// For each provider that needs an API key (Anthropic, OpenAI) but doesn't
/// have one yet, try the provider-specific env var — but ONLY if the
/// provider uses the default API endpoint (no custom `base_url`).
///
/// ## Security: no env-var fallback for custom base_url
///
/// A malicious `dyson.json` checked into a shared repo could define a
/// provider with `base_url` pointing to an attacker's server and no
/// explicit `api_key`.  Without this guard, the loader would inject the
/// victim's real API key from their environment, sending it to the
/// attacker on every request.
///
/// The fix: env-var fallback is only used when `base_url` is `None`
/// (i.e., the provider targets the official API).  Providers with a
/// custom endpoint MUST supply their own `api_key` explicitly.
pub(super) fn resolve_api_keys(settings: &mut Settings, secrets: &SecretRegistry) -> Result<()> {
    // Best-effort: resolve keys for all providers in the map.
    for (name, provider) in settings.providers.iter_mut() {
        let entry = crate::llm::registry::lookup(&provider.provider_type);
        match entry.resolve_api_key(&provider.api_key, &provider.base_url, secrets, false) {
            Ok(Some(key)) => provider.api_key = key,
            Ok(None) => {}
            Err(_) => {
                tracing::debug!(
                    provider = name.as_str(),
                    "no API key for provider (not active, skipping)"
                );
            }
        }
    }

    // Required: resolve the active agent's key.
    let active_entry = crate::llm::registry::lookup(&settings.agent.provider);
    if let Some(key) = active_entry.resolve_api_key(
        &settings.agent.api_key,
        &settings.agent.base_url,
        secrets,
        true, // required — error if missing
    )? {
        settings.agent.api_key = key;
    }

    // SECURITY: reject any provider that would send an API key over plain
    // HTTP to a remote host.  Localhost is fine (Ollama, vLLM, etc.), but
    // a remote HTTP endpoint would transmit the key in cleartext.
    reject_http_with_api_key(
        &settings.agent.base_url,
        settings.agent.api_key.expose(),
        "active agent",
    )?;
    for (name, provider) in &settings.providers {
        reject_http_with_api_key(&provider.base_url, provider.api_key.expose(), name)?;
    }

    Ok(())
}

/// Return a config error if a provider would send an API key over plain
/// HTTP to a non-localhost endpoint.  Keys over HTTP are transmitted in
/// cleartext and can be intercepted by anyone on the network path — loading
/// such a config is a mistake we refuse rather than warn about.
pub(super) fn reject_http_with_api_key(
    base_url: &Option<String>,
    api_key: &str,
    label: &str,
) -> Result<()> {
    let url = match base_url {
        Some(u) => u,
        None => return Ok(()), // Default endpoint — always HTTPS.
    };
    if api_key.is_empty() {
        return Ok(()); // No key to leak.
    }
    if !url.starts_with("http://") {
        return Ok(()); // HTTPS or other scheme — fine.
    }
    // Allow local-network destinations: loopback, RFC1918 private,
    // link-local (covers cube-to-host gateway IPs), and Tailscale's
    // 100.64/10 carrier-grade NAT range.  Plain HTTP on these
    // addresses can't leave the host's local network, so the api_key
    // never crosses an untrusted hop.  Public IPs still get rejected.
    if crate::http::host_from_url(url).is_some_and(is_local_network_host) {
        return Ok(());
    }
    Err(DysonError::Config(format!(
        "provider '{label}' would send an API key over plain HTTP to '{url}' \
         (cleartext on the wire). Use HTTPS or remove the api_key."
    )))
}

/// True for hosts whose traffic stays on the local network (loopback,
/// RFC1918 private, link-local, CGNAT, etc).  Used by
/// `reject_http_with_api_key` to decide whether plain HTTP + api_key is
/// acceptable.  Routes IP literals through the shared SSRF predicates in
/// [`crate::http`] so "what counts as internal" has one definition; plain
/// DNS names default to "remote" (the safe direction).
pub(super) fn is_local_network_host(host: &str) -> bool {
    if host == "localhost" || host == "[::1]" || host == "::1" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => crate::http::is_private_v4(ip),
        Ok(std::net::IpAddr::V6(ip)) => crate::http::is_private_v6(ip),
        Err(_) => false,
    }
}
