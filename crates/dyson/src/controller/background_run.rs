//! Launch and persist background command runs.
use super::{
    AgentMode, BackgroundCompletion, ClientRegistry, CommandResult, background, build_agent,
    completed_text,
};
use crate::config::Settings;

/// Spawn a background agent with unlimited iterations.
///
/// Constructs a fresh agent from the current settings, wires up a
/// `CancellationToken` for `/stop`, and runs it in a `tokio::spawn` task.
/// The agent's conversation is persisted through the existing chat history
/// system under chat ID `bg-<id>`, reusing the same storage and media
/// externalization as regular conversations.
/// Finalise a background agent run: invoke the completion callback (if any)
/// and remove the registry entry.  Extracted so it can be unit-tested without
/// spinning up a real agent / LLM client.
pub(crate) fn finish_background_agent(
    id: u64,
    result: Result<String, String>,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: &Option<BackgroundCompletion>,
) {
    if let Some(cb) = on_complete.as_ref() {
        cb(id, result);
    }
    bg_registry.remove(id);
}

/// Render a background agent's final outcome as transcript text.
///
/// Shared by the Telegram notification path and by the chat-history append
/// so the user sees identical wording in both places.
pub fn format_background_result(id: u64, result: &std::result::Result<String, String>) -> String {
    match result {
        Ok(text) if !text.trim().is_empty() => format!("Background agent #{id} result:\n{text}"),
        Ok(_) => format!("Background agent #{id} finished with no response."),
        Err(e) => format!("Background agent #{id} failed: {e}"),
    }
}

/// Append a background agent's result to a chat's persisted history.
///
/// Loads the stored conversation, pushes a single user message containing
/// the formatted outcome, and writes it back.  Returns the full updated
/// vector so callers with a live in-memory agent can refresh its state in
/// lock-step with the on-disk copy.
pub fn persist_background_result(
    store: &dyn crate::chat_history::ChatHistory,
    chat_key: &str,
    id: u64,
    result: &std::result::Result<String, String>,
) -> crate::error::Result<Vec<crate::message::Message>> {
    let mut msgs = store.load(chat_key)?;
    msgs.push(crate::message::Message::user(&format_background_result(
        id, result,
    )));
    store.save(chat_key, &msgs)?;
    Ok(msgs)
}

pub(crate) async fn spawn_background_agent(
    prompt: &str,
    settings: &Settings,
    registry: &ClientRegistry,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: Option<BackgroundCompletion>,
) -> CommandResult {
    use crate::agent::rate_limiter::Priority;
    use tokio_util::sync::CancellationToken;

    let cancel = CancellationToken::new();

    let prompt_preview = if prompt.len() > 100 {
        format!("{}...", &prompt[..97])
    } else {
        prompt.to_string()
    };

    let id = match bg_registry.allocate(prompt_preview.clone(), cancel.clone()) {
        Ok(id) => id,
        Err(e) => return CommandResult::LoopError(e),
    };

    let chat_id = format!("bg-{id}");

    // Build the background agent with unlimited iterations and Background priority.
    let bg_client = registry.get_default().with_priority(Priority::Background);

    let mut bg_settings = settings.clone();
    bg_settings.agent.max_iterations = usize::MAX;

    let mut bg_agent = match build_agent(
        &bg_settings,
        None,
        AgentMode::Private,
        bg_client,
        registry,
        None,
    )
    .await
    {
        Ok(agent) => agent,
        Err(e) => {
            bg_registry.remove(id);
            return CommandResult::LoopError(format!("failed to build agent: {e}"));
        }
    };

    bg_agent.set_cancellation_token(cancel);

    // Attach chat history so the conversation is persisted through the
    // existing chat store (same backend as Telegram / other controllers).
    if let Ok(store) = crate::chat_history::create_chat_history(&settings.chat_history) {
        let store: std::sync::Arc<dyn crate::chat_history::ChatHistory> =
            std::sync::Arc::from(store);
        bg_agent.set_chat_history(store, chat_id.clone());
    }

    let prompt_owned = prompt.to_string();
    let bg_reg = std::sync::Arc::clone(bg_registry);
    let mut output = crate::agent::SilentOutput;

    let handle = tokio::spawn(async move {
        tracing::info!(id, prompt = %prompt_owned, "background agent starting");
        let result: Result<String, String> = match bg_agent
            .run_detailed(&prompt_owned, &mut output)
            .await
            .and_then(completed_text)
        {
            Ok(text) => {
                tracing::info!(id, text_len = text.len(), "background agent completed");
                Ok(text)
            }
            Err(e) => {
                tracing::warn!(
                    id,
                    error = %e,
                    "background agent failed"
                );
                Err(e.to_string())
            }
        };
        finish_background_agent(id, result, &bg_reg, &on_complete);
    });

    bg_registry.set_handle(id, handle);

    CommandResult::LoopStarted {
        id,
        prompt_preview,
        chat_id,
    }
}
