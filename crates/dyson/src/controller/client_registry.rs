//! Rate-limited clients shared by all interaction channels.
use super::active_provider_name;
use crate::config::Settings;

/// Registry of rate-limited LLM clients, one per configured provider.
///
/// Created once and shared across all controllers via `Arc`.  Clients are
/// created lazily on first access and cached.  This means rate-limit
/// windows survive provider switches and are shared across controllers —
/// switching from Claude to GPT and back doesn't reset the rate counter.
///
/// On config reload, call [`ClientRegistry::reload()`] to swap in new
/// settings.  All cached clients are dropped so subsequent `get()` calls
/// pick up new API keys / base URLs.
pub struct ClientRegistry {
    /// One `RateLimited` per provider name.  Lazily populated on first
    /// `get()` call for that provider.  Behind a `Mutex` for interior
    /// mutability so the registry can be shared via `Arc`.
    inner: std::sync::Mutex<ClientRegistryInner>,
}

struct ClientRegistryInner {
    clients: std::collections::HashMap<
        String,
        crate::agent::rate_limiter::RateLimited<Box<dyn crate::llm::LlmClient>>,
    >,
    /// Settings snapshot used to create clients on demand.
    settings: Settings,
    /// Workspace reference for CLI-subprocess providers (ClaudeCode, Codex).
    workspace: Option<crate::workspace::WorkspaceHandle>,
}

impl ClientRegistry {
    /// Create a new registry from the current settings.
    ///
    /// No clients are created yet — they are lazily instantiated on first
    /// `get()`.  The registry keeps a clone of `settings` so it can build
    /// clients at any time without borrowing from the controller.
    pub fn new(settings: &Settings, workspace: Option<crate::workspace::WorkspaceHandle>) -> Self {
        Self {
            inner: std::sync::Mutex::new(ClientRegistryInner {
                clients: std::collections::HashMap::new(),
                settings: settings.clone(),
                workspace,
            }),
        }
    }

    /// Create a registry with an already-installed default client.
    ///
    /// Test rigs use this to exercise controller behavior without making
    /// network calls to a real LLM provider.
    #[doc(hidden)]
    pub fn new_with_default_client_for_test(
        settings: &Settings,
        client: Box<dyn crate::llm::LlmClient>,
    ) -> Self {
        let mut clients = std::collections::HashMap::new();
        let rate_limited = crate::agent::rate_limiter::RateLimited::unlimited(client);
        if let Some(name) = active_provider_name(settings) {
            clients.insert(name, rate_limited);
        } else {
            clients.insert("__default__".to_string(), rate_limited);
        }

        Self {
            inner: std::sync::Mutex::new(ClientRegistryInner {
                clients,
                settings: settings.clone(),
                workspace: None,
            }),
        }
    }

    /// Drop all cached clients and swap in new settings.
    ///
    /// Subsequent `get()` calls will create new clients with the updated
    /// API keys / base URLs.  Call this on config reload instead of
    /// replacing the entire registry.
    pub fn reload(
        &self,
        settings: &Settings,
        workspace: Option<crate::workspace::WorkspaceHandle>,
    ) {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");
        inner.clients.clear();
        inner.settings = settings.clone();
        inner.workspace = workspace;
    }

    /// Get a `UserFacing` handle to the client for a named provider.
    ///
    /// Creates the client on first access.  Returns `Err` if the provider
    /// name is not in the settings.
    pub fn get(
        &self,
        provider_name: &str,
    ) -> crate::Result<crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>>
    {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");

        if !inner.clients.contains_key(provider_name) {
            let pc = inner.settings.providers.get(provider_name).ok_or_else(|| {
                crate::error::DysonError::Config(format!("unknown provider '{provider_name}'"))
            })?;

            let agent_settings = crate::config::AgentSettings {
                provider: pc.provider_type.clone(),
                api_key: pc.api_key.clone(),
                base_url: pc.base_url.clone(),
                ..inner.settings.agent.clone()
            };

            let client = crate::llm::create_client(
                &agent_settings,
                inner.workspace.clone(),
                inner.settings.sandbox_bypass.as_ref(),
            );

            let rate_limited = match inner.settings.agent.rate_limit.as_ref() {
                Some(rl) => crate::agent::rate_limiter::RateLimited::new(
                    client,
                    rl.max_messages,
                    std::time::Duration::from_secs(rl.window_secs),
                ),
                None => crate::agent::rate_limiter::RateLimited::unlimited(client),
            };

            inner
                .clients
                .insert(provider_name.to_string(), rate_limited);
        }

        let rl = &inner.clients[provider_name];
        Ok(rl.handle(crate::agent::rate_limiter::Priority::UserFacing))
    }

    /// Get a handle for the default (active) provider from settings.
    ///
    /// Looks up the provider name that matches the current agent config,
    /// or falls back to creating a client directly from the agent settings.
    /// HTTP turn admission rejects named-provider configs without an active
    /// provider before reaching this fallback; keep it for legacy/direct
    /// controllers that do not use a `providers` map.
    pub fn get_default(
        &self,
    ) -> crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>> {
        let mut inner = self.inner.lock().expect("ClientRegistry poisoned");

        // Try to find the named provider that matches.
        if let Some(name) = active_provider_name(&inner.settings) {
            // Release the lock temporarily so `get()` can re-acquire it.
            drop(inner);
            if let Ok(handle) = self.get(&name) {
                return handle;
            }
            inner = self.inner.lock().expect("ClientRegistry poisoned");
        }

        // Fallback: create client from the default agent settings.
        // This handles the case where no named provider matches (e.g.
        // single-provider config without a "providers" map).
        if !inner.clients.contains_key("__default__") {
            let client = crate::llm::create_client(
                &inner.settings.agent,
                inner.workspace.clone(),
                inner.settings.sandbox_bypass.as_ref(),
            );
            let rate_limited = match inner.settings.agent.rate_limit.as_ref() {
                Some(rl) => crate::agent::rate_limiter::RateLimited::new(
                    client,
                    rl.max_messages,
                    std::time::Duration::from_secs(rl.window_secs),
                ),
                None => crate::agent::rate_limiter::RateLimited::unlimited(client),
            };
            inner
                .clients
                .insert("__default__".to_string(), rate_limited);
        }

        inner.clients["__default__"].handle(crate::agent::rate_limiter::Priority::UserFacing)
    }
}
