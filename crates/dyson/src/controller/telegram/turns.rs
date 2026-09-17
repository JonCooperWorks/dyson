//! Execute chat turns and record responses, with busy-agent fallback.
use super::{
    ChatEntry,
    api::BotApi,
    attachments::extract_attachments,
    messages::prepend_reply_context,
    output::TelegramOutput,
    types::{self, ChatId},
};
use crate::{config::Settings, controller::Output};
use dyson_telegram::media::DownloadLimits;
use std::sync::Arc;

/// Run the agent for a message in a background task, with quick-response fallback.
pub(super) async fn report_attachment_skips(
    bot: &BotApi,
    chat_id: ChatId,
    text: &str,
    attachments: &[crate::media::Attachment],
    skip_reasons: &[String],
) -> bool {
    if skip_reasons.is_empty() {
        return false;
    }
    let body = skip_reasons.join("\n");
    let _ = bot.send_message(chat_id, &body).await;
    attachments.is_empty() && text.trim().is_empty()
}

pub(super) async fn run_telegram_turn(
    agent: &mut crate::agent::Agent,
    output: &mut TelegramOutput,
    settings: &Settings,
    text: &str,
    attachments: Vec<crate::media::Attachment>,
) -> crate::Result<String> {
    match crate::controller::slash::dispatch_executable(
        agent,
        output,
        settings,
        text,
        !attachments.is_empty(),
    )
    .await?
    {
        crate::controller::slash::SlashDispatch::Handled(_) => Ok(String::new()),
        crate::controller::slash::SlashDispatch::NotSlash
        | crate::controller::slash::SlashDispatch::BuiltinOrUnhandled => {
            if attachments.is_empty() {
                agent
                    .run_detailed(text, output)
                    .await
                    .and_then(crate::controller::completed_text)
            } else {
                agent
                    .run_with_attachments_detailed(text, attachments, output)
                    .await
                    .and_then(crate::controller::completed_text)
            }
        }
    }
}

pub(super) async fn record_telegram_turn(
    entry: &ChatEntry,
    chat_store: &dyn crate::chat_history::ChatHistory,
    chat_key: &str,
    messages: Vec<crate::message::Message>,
    sent_ids: &[types::MessageId],
) {
    if let Some(turn_index) = messages
        .iter()
        .rposition(|message| message.role == crate::message::Role::Assistant)
    {
        let mut id_map = entry.message_id_map.write().await;
        for message_id in sent_ids {
            id_map.insert(message_id.0, turn_index);
        }
    }
    if let Err(error) = chat_store.save(chat_key, &messages) {
        tracing::error!(error = %error, "failed to save chat history");
    }
    *entry.messages_snapshot.write().await = messages;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_agent_for_message(
    bot: BotApi,
    chat_id: ChatId,
    msg: types::Message,
    text: String,
    entry: Arc<ChatEntry>,
    chat_store: Arc<dyn crate::chat_history::ChatHistory>,
    client: crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
    download_limits: Arc<DownloadLimits>,
    settings: Settings,
) {
    let chat_key = chat_id.0.to_string();

    let text = prepend_reply_context(&msg, text);

    // try_lock() is the gate: if the agent is busy (or temporarily extracted
    // by handle_per_chat_command), fall back to a quick response.
    let mut ca = match entry.agent.try_lock() {
        Ok(guard) if guard.agent.is_some() => guard,
        _ => {
            tracing::info!(chat_id = chat_id.0, "agent busy — using quick response");
            send_quick_response(&bot, chat_id, &text, &entry, &client).await;
            return;
        }
    };

    let (attachments, skip_reasons) = extract_attachments(&bot, &msg, &download_limits).await;

    // If the user only sent unsupported content (e.g. a binary document with
    // no caption) and nothing else to work with, reply with the skip reasons
    // and bail out before invoking the agent.
    if report_attachment_skips(&bot, chat_id, &text, &attachments, &skip_reasons).await {
        return;
    }

    let agent = ca.agent.as_mut().expect("checked above");

    // Set attribution for write auditing in public agents.
    // Uses the sender's @username, falling back to their numeric user ID.
    let sender_label = msg
        .from
        .as_ref()
        .map(|u| u.username.clone().unwrap_or_else(|| u.id.to_string()));
    agent.set_attribution(sender_label.as_deref()).await;

    // Update snapshot so quick responses see latest context.
    *entry.messages_snapshot.write().await = agent.messages().to_vec();

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let result = run_telegram_turn(agent, &mut output, &settings, &text, attachments).await;

    if let Err(e) = result {
        tracing::error!(error = %e, "agent run failed");
        let _ = output.error(&e);
    }

    // Clear attribution so background dreams don't inherit a stale user.
    agent.set_attribution(None).await;

    // Snapshot messages, then release the lock before I/O.
    let agent = ca.agent.as_ref().expect("checked above");
    let msgs = agent.messages().to_vec();
    drop(ca);

    // Record which Telegram message IDs correspond to this assistant turn.
    // This lets us map emoji reactions back to the conversation turn index.
    record_telegram_turn(
        &entry,
        chat_store.as_ref(),
        &chat_key,
        msgs,
        output.sent_message_ids(),
    )
    .await;
}

/// Send a quick response (no tools, fast) when the agent is busy.
///
/// Uses the shared client handle — no new LLM client is created.
async fn send_quick_response(
    bot: &BotApi,
    chat_id: ChatId,
    text: &str,
    entry: &ChatEntry,
    client: &crate::agent::rate_limiter::RateLimitedHandle<Box<dyn crate::llm::LlmClient>>,
) {
    let messages_snap = entry.messages_snapshot.read().await.clone();
    let sys_prompt = entry.system_prompt.read().await.clone();
    let config = entry.config.read().await.clone();

    let llm_client = match client.access() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::warn!(error = %e, "quick response rate-limited");
            return;
        }
    };

    let mut output = TelegramOutput::new(bot.clone(), chat_id, !text.is_empty());

    let result = crate::agent::quick_response(
        &**llm_client,
        &messages_snap,
        &sys_prompt,
        text,
        &config,
        &mut output,
    )
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "quick response failed");
        let _ = output.error(&e);
    }
}
