//! Per-chat state, creation, eviction, and reload.
use super::{MAX_CHAT_ENTRIES, MAX_IN_FLIGHT_PER_CHAT, WARMUP_PLACEHOLDER, epoch_secs};
use crate::config::Settings;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

/// Per-chat agent state, tracking the active provider and model
/// so within-provider model switching works.
///
/// `agent` is `Option` so it can be temporarily extracted (`.take()`) to
/// release the mutex during long-running operations like `/compact`.
pub(super) struct ChatAgent {
    pub(super) agent: Option<crate::agent::Agent>,
    pub(super) provider_name: String,
    pub(super) model: String,
}

/// Per-chat entry with its own lock and quick-response state.
///
/// This replaces the old global `Mutex<HashMap<i64, ChatAgent>>` design.
/// Each chat gets its own mutex so that:
/// 1. Different chats never block each other.
/// 2. When a chat's agent is locked, new messages get a quick response
///    (single LLM call, no tools) via `try_lock()` instead of blocking.
pub(super) struct ChatEntry {
    /// The agent, behind its own mutex (locked only during agent.run()).
    /// `try_lock()` is the gate: if it fails, the agent is busy and we
    /// fall back to a quick response.
    pub(super) agent: Mutex<ChatAgent>,
    /// Snapshot of conversation messages, updated before each agent run.
    /// Quick response reads this when the agent is busy.
    pub(super) messages_snapshot: tokio::sync::RwLock<Vec<crate::message::Message>>,
    /// System prompt for quick response context.
    pub(super) system_prompt: tokio::sync::RwLock<String>,
    /// Completion config for quick response LLM calls.
    pub(super) config: tokio::sync::RwLock<crate::llm::CompletionConfig>,
    /// Whether this chat is a group/supergroup.
    /// Group chats run as public agents with per-channel workspace
    /// (workspace memory + web tools, no filesystem/shell).
    pub(super) is_group: bool,
    /// Maps Telegram message IDs (sent by the bot) → conversation turn index.
    /// Used to associate emoji reactions with the correct assistant response.
    /// In-memory only — rebuilt each session.  Cleared on `/clear`.
    pub(super) message_id_map: tokio::sync::RwLock<HashMap<i32, usize>>,
    /// When this chat last received a message.  Used for LRU eviction.
    pub(super) last_active: std::sync::atomic::AtomicI64,
    /// Bounds the number of in-flight tasks per chat so a single user
    /// flooding the bot cannot spawn unlimited background LLM calls.
    /// `try_acquire_owned` is non-blocking: if no permit is available
    /// the message is dropped with a log line rather than queueing.
    pub(super) in_flight: Arc<tokio::sync::Semaphore>,
}

pub(super) async fn rebuild_agents_on_reload(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    settings: &Settings,
    controller_prompt: Option<&str>,
    chat_store: &Arc<dyn crate::chat_history::ChatHistory>,
    feedback_dir: &std::path::Path,
    registry: &crate::controller::ClientRegistry,
) {
    let mut agents_map = agents.write().await;
    let old_agents: Vec<(i64, Arc<ChatEntry>)> = agents_map.drain().collect();
    for (chat_id, entry) in old_agents {
        let ca = entry.agent.lock().await;
        let previous_provider_name = ca.provider_name.clone();
        let previous_model = ca.model.clone();
        let messages = ca
            .agent
            .as_ref()
            .expect("agent not available")
            .messages()
            .to_vec();
        let is_group = entry.is_group;
        drop(ca);
        let (provider_name, model) =
            reloaded_provider_model(settings, &previous_provider_name, &previous_model, is_group);

        // Both public and private agents are rebuilt from scratch on config
        // reload (cheap — allocation is fine here).  Private agents with a
        // non-default provider/model get a swap_client after building.
        let default_client = registry.get_default();
        let mode = if is_group {
            crate::controller::AgentMode::Public
        } else {
            crate::controller::AgentMode::Private
        };
        let channel_id_str = chat_id.to_string();
        let ch = if is_group {
            Some(channel_id_str.as_str())
        } else {
            None
        };
        let agent_result = crate::controller::build_agent(
            settings,
            controller_prompt,
            mode,
            default_client,
            registry,
            ch,
        )
        .await
        .map(|mut a| {
            a.set_messages(messages.clone());
            // If this private agent was using a non-default provider, swap
            // to the correct client from the registry.
            if !is_group
                && let Some(pc) = settings.providers.get(&provider_name)
                && let Ok(handle) = registry.get(&provider_name)
            {
                a.swap_client(handle, &model, &pc.provider_type);
            }
            a
        });

        match agent_result {
            Ok(mut new_agent) => {
                new_agent.set_chat_history(Arc::clone(chat_store), chat_id.to_string());
                new_agent.set_feedback_store(crate::feedback::FeedbackStore::new(
                    feedback_dir.to_path_buf(),
                ));
                let sys_prompt = new_agent.system_prompt().to_string();
                let cfg = new_agent.config().clone();
                agents_map.insert(
                    chat_id,
                    Arc::new(ChatEntry {
                        agent: Mutex::new(ChatAgent {
                            agent: Some(new_agent),
                            provider_name,
                            model,
                        }),
                        messages_snapshot: tokio::sync::RwLock::new(messages),
                        system_prompt: tokio::sync::RwLock::new(sys_prompt),
                        config: tokio::sync::RwLock::new(cfg),
                        is_group,
                        message_id_map: tokio::sync::RwLock::new(HashMap::new()),
                        last_active: std::sync::atomic::AtomicI64::new(epoch_secs()),
                        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CHAT)),
                    }),
                );
            }
            Err(e) => {
                tracing::warn!(
                    chat_id,
                    provider = provider_name,
                    model,
                    is_group,
                    error = %e,
                    "could not rebuild agent after reload — dropping",
                );
            }
        }
    }
    drop(agents_map);
    tracing::info!("config/workspace reloaded — agents rebuilt");
}

pub(super) fn reloaded_provider_model(
    settings: &Settings,
    previous_provider_name: &str,
    previous_model: &str,
    is_group: bool,
) -> (String, String) {
    let default_provider = crate::controller::active_provider_name(settings).unwrap_or_default();
    let default_model = settings.agent.model.clone();
    if is_group {
        return (default_provider, default_model);
    }
    let previous_model = previous_model.trim();
    let can_keep_previous = !previous_provider_name.trim().is_empty()
        && !previous_model.is_empty()
        && previous_model != WARMUP_PLACEHOLDER
        && settings
            .providers
            .get(previous_provider_name)
            .is_some_and(|pc| pc.models.iter().any(|m| m == previous_model));
    if can_keep_previous {
        (previous_provider_name.to_owned(), previous_model.to_owned())
    } else {
        (default_provider, default_model)
    }
}

/// Takes a read lock on the map first (fast path).  Only upgrades to a write
/// lock if the entry doesn't exist yet.  This means existing chats never
/// contend on the map lock — only the first message in a new chat takes the
/// write lock briefly.
/// Shared context for creating new chat entries.
pub(super) struct ChatContext<'a> {
    pub(super) settings: &'a Settings,
    pub(super) controller_prompt: Option<&'a str>,
    pub(super) chat_store: &'a Arc<dyn crate::chat_history::ChatHistory>,
    pub(super) feedback_dir: &'a std::path::Path,
    pub(super) registry: &'a crate::controller::ClientRegistry,
}

pub(super) async fn get_or_create_entry(
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
    chat_id: i64,
    is_group: bool,
    cx: &ChatContext<'_>,
) -> crate::Result<Arc<ChatEntry>> {
    // Fast path: entry already exists.
    {
        let map = agents.read().await;
        if let Some(entry) = map.get(&chat_id) {
            entry
                .last_active
                .store(epoch_secs(), std::sync::atomic::Ordering::Relaxed);
            return Ok(Arc::clone(entry));
        }
    }

    // Slow path: create a new agent for this chat.
    let mode = if is_group {
        crate::controller::AgentMode::Public
    } else {
        crate::controller::AgentMode::Private
    };
    let client = cx.registry.get_default();
    let chat_key = chat_id.to_string();
    let ch = if is_group {
        Some(chat_key.as_str())
    } else {
        None
    };
    let mut agent = crate::controller::build_agent(
        cx.settings,
        cx.controller_prompt,
        mode,
        client,
        cx.registry,
        ch,
    )
    .await?;

    // Attach chat history so compaction can rotate pre-compaction snapshots.
    agent.set_chat_history(Arc::clone(cx.chat_store), chat_key.clone());
    agent.set_feedback_store(crate::feedback::FeedbackStore::new(
        cx.feedback_dir.to_path_buf(),
    ));

    let mut restored_messages = Vec::new();
    if let Ok(messages) = cx.chat_store.load(&chat_key)
        && !messages.is_empty()
    {
        tracing::info!(chat_id, messages = messages.len(), "restored chat history");
        agent.set_messages(messages.clone());
        restored_messages = messages;
    }

    let provider_name = crate::controller::active_provider_name(cx.settings).unwrap_or_default();
    let model = cx.settings.agent.model.clone();
    let sys_prompt = agent.system_prompt().to_string();
    let config = agent.config().clone();

    let entry = Arc::new(ChatEntry {
        agent: Mutex::new(ChatAgent {
            agent: Some(agent),
            provider_name,
            model,
        }),
        messages_snapshot: tokio::sync::RwLock::new(restored_messages),
        system_prompt: tokio::sync::RwLock::new(sys_prompt),
        config: tokio::sync::RwLock::new(config),
        is_group,
        message_id_map: tokio::sync::RwLock::new(HashMap::new()),
        last_active: std::sync::atomic::AtomicI64::new(epoch_secs()),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_PER_CHAT)),
    });

    // Evict the least-recently-active entry if we're at capacity.
    // Conversation history is already persisted to chat_store on every
    // turn, so the evicted chat can be fully restored on next message.
    let mut map = agents.write().await;
    if map.len() >= MAX_CHAT_ENTRIES
        && !map.contains_key(&chat_id)
        && let Some((&victim_id, _)) = map
            .iter()
            .min_by_key(|(_, e)| e.last_active.load(std::sync::atomic::Ordering::Relaxed))
    {
        tracing::info!(
            evicted_chat_id = victim_id,
            active_chats = map.len(),
            "evicting least-recently-active chat entry"
        );
        map.remove(&victim_id);
    }
    let entry = Arc::clone(map.entry(chat_id).or_insert_with(|| Arc::clone(&entry)));
    Ok(entry)
}
