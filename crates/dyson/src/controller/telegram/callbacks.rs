//! Inline model/stop callbacks and emoji feedback.
use super::{
    ChatEntry,
    api::BotApi,
    types::{self, ChatId},
};
use crate::config::Settings;
use std::{collections::HashMap, sync::Arc};

/// Handle a callback query (inline keyboard button press).
///
/// Dispatches on the callback data prefix:
///   - `model:{provider}:{model}` — hot-swap to the selected model.
///   - `stop_agent:{id}` — cancel the selected background agent.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_callback_query(
    bot: &BotApi,
    cb_data: &str,
    chat_id: ChatId,
    agents: &Arc<tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>>,
    settings: &Settings,
    config_path: Option<&std::path::Path>,
    registry: &crate::controller::ClientRegistry,
    bg_registry: &std::sync::Arc<crate::controller::background::BackgroundAgentRegistry>,
) {
    if let Some(rest) = cb_data.strip_prefix("stop_agent:") {
        let id: u64 = match rest.parse() {
            Ok(id) => id,
            Err(_) => {
                let _ = bot.send_message(chat_id, "Invalid agent ID").await;
                return;
            }
        };
        match bg_registry.stop(id) {
            Ok(()) => {
                let _ = bot
                    .send_message(chat_id, &format!("Agent #{id} stopped."))
                    .await;
            }
            Err(e) => {
                let _ = bot.send_message(chat_id, &format!("Stop error: {e}")).await;
            }
        }
        return;
    }

    let Some(rest) = cb_data.strip_prefix("model:") else {
        return;
    };
    let Some((provider, model)) = rest.split_once(':') else {
        return;
    };

    let pc = match settings.providers.get(provider) {
        Some(pc) => pc,
        None => {
            let _ = bot
                .send_message(chat_id, &format!("Unknown provider '{provider}'"))
                .await;
            return;
        }
    };

    let handle = match registry.get(provider) {
        Ok(h) => h,
        Err(e) => {
            let _ = bot
                .send_message(chat_id, &format!("Switch error: {e}"))
                .await;
            return;
        }
    };

    if let Err(error) = crate::swarm_state_sync::persist_model_selection(provider, model).await {
        tracing::warn!(
            error = %error,
            provider,
            model,
            "telegram model switch was not persisted by swarm"
        );
        let _ = bot
            .send_message(
                chat_id,
                "Swarm could not save that model selection; the active model was not changed.",
            )
            .await;
        return;
    }

    // Hot-swap the client on the existing agent — no rebuild needed.
    let agents_map = agents.read().await;
    if let Some(entry) = agents_map.get(&chat_id.0) {
        let mut ca = entry.agent.lock().await;
        let agent = ca.agent.as_mut().expect("agent not available");
        agent.swap_client(handle, model, &pc.provider_type);
        ca.provider_name = provider.to_string();
        ca.model = model.to_string();
        // Update cached state for quick responses.
        let agent = ca.agent.as_ref().expect("agent not available");
        *entry.system_prompt.write().await = agent.system_prompt().to_string();
        *entry.config.write().await = agent.config().clone();
    }
    drop(agents_map);

    if let Some(cp) = config_path {
        crate::config::loader::persist_model_selection(cp, provider, model);
    }
    let reply = format!(
        "Switched to '{}' — {:?} ({})",
        provider, pc.provider_type, model,
    );
    let _ = bot.send_message(chat_id, &reply).await;
}

/// Handle a message_reaction update — record emoji feedback for RLHF.
///
/// Converts the Telegram-specific emoji reaction into a domain-level
/// `FeedbackEntry` before passing it to the store.
pub(super) async fn handle_reaction(
    reaction: &types::MessageReactionUpdated,
    agents: &tokio::sync::RwLock<HashMap<i64, Arc<ChatEntry>>>,
) {
    let chat_id = reaction.chat.id;
    let message_id = reaction.message_id;

    // Look up the chat entry to find the message_id → turn_index mapping.
    let agents_map = agents.read().await;
    let entry = match agents_map.get(&chat_id) {
        Some(e) => Arc::clone(e),
        None => {
            tracing::debug!(chat_id, message_id, "reaction on unknown chat — ignoring");
            return;
        }
    };
    drop(agents_map);

    let id_map = entry.message_id_map.read().await;
    let turn_index = match id_map.get(&message_id) {
        Some(&idx) => idx,
        None => {
            tracing::debug!(
                chat_id,
                message_id,
                "reaction on unmapped message — ignoring"
            );
            return;
        }
    };
    drop(id_map);

    // Lock the agent to record feedback.
    let ca = entry.agent.lock().await;
    let Some(ref agent) = ca.agent else {
        tracing::debug!(chat_id, "agent not available for feedback");
        return;
    };

    // Empty new_reaction means the user removed their reaction.
    if reaction.new_reaction.is_empty() {
        if let Err(e) = agent.remove_feedback(turn_index) {
            tracing::warn!(error = %e, chat_id, turn_index, "failed to remove feedback");
        } else {
            tracing::info!(chat_id, turn_index, "feedback removed (reaction cleared)");
        }
        return;
    }

    // Extract the first standard emoji from the reaction.
    let emoji = reaction.new_reaction.iter().find_map(|r| {
        if r.reaction_type == "emoji" {
            r.emoji.as_deref()
        } else {
            None
        }
    });

    let Some(emoji) = emoji else {
        tracing::debug!(
            chat_id,
            message_id,
            "reaction has no standard emoji — ignoring"
        );
        return;
    };

    // Convert emoji → rating at the Telegram boundary.
    let Some(rating) = crate::feedback::FeedbackRating::from_emoji(emoji) else {
        tracing::debug!(chat_id, emoji, "unknown emoji reaction — ignoring");
        return;
    };

    if let Err(e) = agent.record_feedback(turn_index, rating) {
        tracing::warn!(error = %e, chat_id, turn_index, "failed to save feedback");
    } else {
        tracing::info!(chat_id, turn_index, emoji, "feedback recorded");
    }
}
