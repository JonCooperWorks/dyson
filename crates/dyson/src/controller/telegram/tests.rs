use super::chats::reloaded_provider_model;
use super::commands::{build_agents_keyboard, truncate_chars};
use super::messages::{
    is_bot_mentioned, is_directed_group_msg, is_reply_to_bot, prepend_reply_context, should_reject,
    should_reject_group_command,
};
use super::*;
use crate::controller::Controller;
use dyson_telegram::media::{DocumentKind, classify_document, extension_of};
use types::{Chat, ChatType, Message, User};

fn make_msg(text: &str, chat_type: ChatType) -> Message {
    Message {
        message_id: 1,
        chat: Chat { id: 100, chat_type },
        from: None,
        text: Some(text.to_string()),
        caption: None,
        entities: None,
        reply_to_message: None,
        photo: None,
        voice: None,
        document: None,
    }
}

fn make_group_msg(text: &str) -> Message {
    make_msg(text, ChatType::Supergroup)
}

#[test]
fn classify_image_mime() {
    assert_eq!(
        classify_document("image/png", Some("a.png")),
        DocumentKind::Image
    );
    assert_eq!(classify_document("image/jpeg", None), DocumentKind::Image);
}

#[test]
fn classify_pdf() {
    assert_eq!(
        classify_document("application/pdf", Some("paper.pdf")),
        DocumentKind::Pdf
    );
}

#[test]
fn classify_text_mime() {
    assert_eq!(
        classify_document("text/plain", Some("note.txt")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("text/markdown", Some("README.md")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("application/json", Some("pkg.json")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("application/x-yaml", Some("c.yaml")),
        DocumentKind::Text
    );
}

#[test]
fn classify_octet_stream_by_extension() {
    assert_eq!(
        classify_document("application/octet-stream", Some("main.rs")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("", Some("Cargo.toml")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("application/octet-stream", Some("x.py")),
        DocumentKind::Text
    );
    assert_eq!(
        classify_document("application/octet-stream", Some("deploy.sh")),
        DocumentKind::Text
    );
}

#[test]
fn classify_office_mime() {
    assert_eq!(
        classify_document(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Some("doc.docx")
        ),
        DocumentKind::Office
    );
    assert_eq!(
        classify_document(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("data.xlsx")
        ),
        DocumentKind::Office
    );
    assert_eq!(
        classify_document(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            Some("deck.pptx")
        ),
        DocumentKind::Office
    );
}

#[test]
fn classify_office_by_extension() {
    assert_eq!(
        classify_document("application/octet-stream", Some("report.docx")),
        DocumentKind::Office
    );
    assert_eq!(
        classify_document("", Some("budget.xlsx")),
        DocumentKind::Office
    );
    assert_eq!(
        classify_document("application/octet-stream", Some("slides.pptx")),
        DocumentKind::Office
    );
}

#[test]
fn classify_binary_defaults() {
    assert_eq!(
        classify_document("application/zip", Some("a.zip")),
        DocumentKind::Binary
    );
    assert_eq!(
        classify_document("video/mp4", Some("clip.mp4")),
        DocumentKind::Binary
    );
    assert_eq!(
        classify_document("application/octet-stream", Some("thing.bin")),
        DocumentKind::Binary
    );
    assert_eq!(classify_document("", None), DocumentKind::Binary);
    assert_eq!(
        classify_document("application/octet-stream", None),
        DocumentKind::Binary
    );
}

#[test]
fn extension_extraction() {
    assert_eq!(extension_of("README.md"), Some("md".into()));
    assert_eq!(extension_of("foo.TAR.GZ"), Some("gz".into()));
    assert_eq!(extension_of("nodot"), None);
    assert_eq!(extension_of("trailing."), None);
}

#[test]
fn mention_exact_match() {
    let msg = make_group_msg("@dysonbot hello");
    assert!(is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn mention_case_insensitive() {
    let msg = make_group_msg("@DysonBot hello");
    assert!(is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn mention_mid_text() {
    let msg = make_group_msg("hey @dysonbot what's up");
    assert!(is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn mention_end_of_text() {
    let msg = make_group_msg("hey @dysonbot");
    assert!(is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn mention_not_substring_of_longer_name() {
    let msg = make_group_msg("@dysonbot_extra hello");
    assert!(!is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn mention_empty_username() {
    let msg = make_group_msg("@dysonbot hello");
    assert!(!is_bot_mentioned(&msg, ""));
}

#[test]
fn mention_no_mention() {
    let msg = make_group_msg("just chatting");
    assert!(!is_bot_mentioned(&msg, "dysonbot"));
}

#[test]
fn reply_to_bot_detected() {
    let mut msg = make_group_msg("thanks");
    msg.reply_to_message = Some(Box::new(Message {
        message_id: 0,
        chat: msg.chat.clone(),
        from: Some(User {
            id: 42,
            is_bot: true,
            username: Some("dysonbot".to_string()),
        }),
        text: Some("here's the answer".to_string()),
        caption: None,
        entities: None,
        reply_to_message: None,
        photo: None,
        voice: None,
        document: None,
    }));
    assert!(is_reply_to_bot(&msg, 42));
}

#[test]
fn reply_to_human_not_detected() {
    let mut msg = make_group_msg("thanks");
    msg.reply_to_message = Some(Box::new(Message {
        message_id: 0,
        chat: msg.chat.clone(),
        from: Some(User {
            id: 99,
            is_bot: false,
            username: Some("alice".to_string()),
        }),
        text: Some("some message".to_string()),
        caption: None,
        entities: None,
        reply_to_message: None,
        photo: None,
        voice: None,
        document: None,
    }));
    assert!(!is_reply_to_bot(&msg, 42));
}

#[test]
fn no_reply_not_detected() {
    let msg = make_group_msg("hello");
    assert!(!is_reply_to_bot(&msg, 42));
}

// -------------------------------------------------------------------
// Chat::is_group — mode-selection input for AgentMode::Public
// -------------------------------------------------------------------

#[test]
fn is_group_for_group_chat() {
    let chat = Chat {
        id: 1,
        chat_type: ChatType::Group,
    };
    assert!(
        chat.is_group(),
        "Group chats should be identified as groups"
    );
}

#[test]
fn is_group_for_supergroup_chat() {
    let chat = Chat {
        id: 1,
        chat_type: ChatType::Supergroup,
    };
    assert!(
        chat.is_group(),
        "Supergroup chats should be identified as groups"
    );
}

#[test]
fn is_group_false_for_private_chat() {
    let chat = Chat {
        id: 1,
        chat_type: ChatType::Private,
    };
    assert!(
        !chat.is_group(),
        "Private chats should not be identified as groups"
    );
}

#[test]
fn is_group_false_for_channel() {
    let chat = Chat {
        id: 1,
        chat_type: ChatType::Channel,
    };
    assert!(
        !chat.is_group(),
        "Channels should not be identified as groups"
    );
}

// -------------------------------------------------------------------
// Group chat mode → AgentMode::Public mapping
// -------------------------------------------------------------------

#[test]
fn group_chat_maps_to_public_mode() {
    // Replicate the mode selection logic from get_or_create_entry.
    let is_group = true;
    let mode = if is_group {
        crate::controller::AgentMode::Public
    } else {
        crate::controller::AgentMode::Private
    };
    assert_eq!(mode, crate::controller::AgentMode::Public);
}

#[test]
fn private_chat_maps_to_private_mode() {
    let is_group = false;
    let mode = if is_group {
        crate::controller::AgentMode::Public
    } else {
        crate::controller::AgentMode::Private
    };
    assert_eq!(mode, crate::controller::AgentMode::Private);
}

// -------------------------------------------------------------------
// Access control: group chats bypass allowed_chat_ids
// -------------------------------------------------------------------

#[test]
fn group_chat_bypasses_allowed_ids() {
    // Group chats are never rejected, even if not in allowed_ids.
    assert!(!should_reject(true, &[111, 222], 999));
}

#[test]
fn group_chat_allowed_with_empty_allowlist() {
    assert!(!should_reject(true, &[], 999));
}

#[test]
fn private_chat_rejected_when_not_in_allowlist() {
    assert!(should_reject(false, &[111, 222], 999));
}

#[test]
fn private_chat_allowed_when_in_allowlist() {
    assert!(!should_reject(false, &[111, 222], 222));
}

#[test]
fn private_chat_allowed_with_empty_allowlist() {
    // Empty allowlist = allow all private chats (guarded by allow_all_chats at init).
    assert!(!should_reject(false, &[], 999));
}

// -------------------------------------------------------------------
// Operator-only commands in group chats
// -------------------------------------------------------------------

#[test]
fn group_command_from_non_operator_rejected() {
    // A random user (id=999) NOT in the allowed list sends /clear in a group.
    assert!(should_reject_group_command(
        true,
        "/clear",
        Some(999),
        &[111, 222],
    ));
}

#[test]
fn group_command_from_operator_allowed() {
    // An operator (id=111) in the allowed list sends /clear in a group.
    assert!(!should_reject_group_command(
        true,
        "/clear",
        Some(111),
        &[111, 222],
    ));
}

#[test]
fn group_command_from_unknown_sender_rejected() {
    // No `from` field at all — should be rejected.
    assert!(should_reject_group_command(
        true,
        "/logs",
        None,
        &[111, 222],
    ));
}

#[test]
fn group_plain_message_from_non_operator_allowed() {
    // Non-operator sends a regular message (not a command) in a group.
    assert!(!should_reject_group_command(
        true,
        "hello bot",
        Some(999),
        &[111, 222],
    ));
}

#[test]
fn private_command_unaffected_by_operator_check() {
    // In private chats the operator gate does not apply.
    assert!(!should_reject_group_command(
        false,
        "/clear",
        Some(999),
        &[111, 222],
    ));
}

#[test]
fn group_whoami_allowed_for_non_operator() {
    // /whoami is a public command, allowed for everyone even in groups.
    assert!(!should_reject_group_command(
        true,
        "/whoami",
        Some(999),
        &[111, 222],
    ));
}

#[test]
fn group_command_all_commands_restricted_for_non_operator() {
    let commands = [
        "/logs",
        "/logs 50",
        "/memory",
        "/memory some note",
        "/clear",
        "/compact",
        "/model provider",
        "/models",
    ];
    for cmd in commands {
        assert!(
            should_reject_group_command(true, cmd, Some(999), &[111, 222]),
            "{cmd} should be rejected for non-operators in groups",
        );
    }
}

fn settings_with_models(models: &[&str]) -> Settings {
    let mut settings = Settings::default();
    settings.agent.provider = crate::config::LlmProvider::OpenRouter;
    settings.agent.model = models[0].to_string();
    settings.active_provider = crate::config::ActiveProvider::new("openrouter", models[0]);
    settings.providers.insert(
        "openrouter".into(),
        crate::config::ProviderConfig {
            provider_type: crate::config::LlmProvider::OpenRouter,
            models: models.iter().map(|m| (*m).to_string()).collect(),
            api_key: crate::auth::Credential::new("token".into()),
            base_url: Some("https://example.test".into()),
        },
    );
    settings
}

#[test]
fn reload_does_not_preserve_warmup_placeholder_model() {
    let settings = settings_with_models(&["deepseek/deepseek-v4-flash"]);
    let (provider, model) =
        reloaded_provider_model(&settings, "openrouter", WARMUP_PLACEHOLDER, false);
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "deepseek/deepseek-v4-flash");
}

#[test]
fn reload_preserves_valid_private_chat_model_choice() {
    let settings = settings_with_models(&["deepseek/deepseek-v4-flash", "qwen/qwen3.6-plus"]);
    let (provider, model) =
        reloaded_provider_model(&settings, "openrouter", "qwen/qwen3.6-plus", false);
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "qwen/qwen3.6-plus");
}

#[test]
fn reload_group_chat_uses_default_model() {
    let settings = settings_with_models(&["deepseek/deepseek-v4-flash"]);
    let (provider, model) =
        reloaded_provider_model(&settings, "openrouter", "qwen/qwen3.6-plus", true);
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "deepseek/deepseek-v4-flash");
}

// -------------------------------------------------------------------
// Group message direction filtering
// -------------------------------------------------------------------

#[test]
fn group_command_is_directed() {
    let msg = make_group_msg("/help");
    assert!(is_directed_group_msg(
        &msg,
        msg.text.as_deref().unwrap_or_default(),
        "dysonbot",
        42
    ));
}

#[test]
fn group_mention_is_directed() {
    let msg = make_group_msg("hey @dysonbot what's the weather?");
    assert!(is_directed_group_msg(
        &msg,
        msg.text.as_deref().unwrap_or_default(),
        "dysonbot",
        42
    ));
}

#[test]
fn group_reply_to_bot_is_directed() {
    let mut msg = make_group_msg("thanks");
    msg.reply_to_message = Some(Box::new(Message {
        message_id: 0,
        chat: msg.chat.clone(),
        from: Some(User {
            id: 42,
            is_bot: true,
            username: Some("dysonbot".to_string()),
        }),
        text: Some("previous answer".to_string()),
        caption: None,
        entities: None,
        reply_to_message: None,
        photo: None,
        voice: None,
        document: None,
    }));
    assert!(is_directed_group_msg(
        &msg,
        msg.text.as_deref().unwrap_or_default(),
        "dysonbot",
        42
    ));
}

#[test]
fn group_undirected_message_not_directed() {
    let msg = make_group_msg("just chatting with friends");
    assert!(!is_directed_group_msg(
        &msg,
        msg.text.as_deref().unwrap_or_default(),
        "dysonbot",
        42
    ));
}

// -------------------------------------------------------------------
// Public command (/whoami) — available to all chats
// -------------------------------------------------------------------

#[test]
fn whoami_is_public_command() {
    assert!(formatting::is_public_command("/whoami"));
}

#[test]
fn other_commands_are_not_public() {
    assert!(!formatting::is_public_command("/help"));
    assert!(!formatting::is_public_command("/clear"));
    assert!(!formatting::is_public_command("/model"));
    assert!(!formatting::is_public_command("whoami"));
}

// -------------------------------------------------------------------
// Telegram controller prompt injected alongside identity
// -------------------------------------------------------------------

#[test]
fn telegram_prompt_coexists_with_identity_in_public_agent() {
    use crate::workspace::Workspace;

    // Public agents now get identity via workspace.system_prompt(),
    // which is composed from SOUL.md and IDENTITY.md by the workspace.
    // Here we verify the prompt composition order works correctly.
    let ws = crate::workspace::InMemoryWorkspace::new()
        .with_file("SOUL.md", "I speak like a pirate.")
        .with_file("IDENTITY.md", "I am Captain Bot.");

    let mut agent_settings = crate::config::AgentSettings::default();

    // Workspace system prompt provides identity.
    let ws_prompt = ws.system_prompt();
    if !ws_prompt.is_empty() {
        agent_settings.system_prompt.push_str("\n\n");
        agent_settings.system_prompt.push_str(&ws_prompt);
    }

    // Public-agent suffix.
    agent_settings
        .system_prompt
        .push_str("\n\nYou are a public-facing agent.");

    // Telegram controller prompt.
    let telegram_prompt = "You are responding via Telegram. Keep these rules:\n\
         - Keep responses concise. Telegram messages have a 4096 character limit.";
    agent_settings.system_prompt.push_str("\n\n");
    agent_settings.system_prompt.push_str(telegram_prompt);

    let prompt = &agent_settings.system_prompt;
    // Identity content from workspace system prompt.
    assert!(
        prompt.contains("I speak like a pirate."),
        "should contain SOUL.md content"
    );
    assert!(
        prompt.contains("I am Captain Bot."),
        "should contain IDENTITY.md content"
    );
    // Public-agent suffix present.
    assert!(
        prompt.contains("public-facing agent"),
        "should contain public suffix"
    );
    // Telegram controller prompt present.
    assert!(
        prompt.contains("Telegram"),
        "should contain Telegram prompt"
    );
    assert!(prompt.contains("4096"), "should mention character limit");
}

#[test]
fn telegram_controller_has_system_prompt() {
    // Verify the TelegramController always provides a system prompt
    // that will be appended to both private and public agents.
    let ctrl = TelegramController {
        bot_token: Some(crate::auth::Credential::new("test".into())),
        proxy: None,
        mode: TelegramMode::Polling,
        allowed_chat_ids: vec![],
        download_limits: Arc::new(DownloadLimits::default()),
    };
    let prompt = ctrl
        .system_prompt()
        .expect("Telegram controller must provide a system prompt");
    assert!(prompt.contains("Telegram"), "should reference Telegram");
    assert!(
        prompt.contains("4096"),
        "should mention the message character limit"
    );
}

// -------------------------------------------------------------------
// Reply context — prepend_reply_context
// -------------------------------------------------------------------

/// Helper to build a reply Message with the given text and sender username.
fn make_reply(text: Option<&str>, username: Option<&str>) -> Message {
    Message {
        message_id: 0,
        chat: Chat {
            id: 100,
            chat_type: ChatType::Private,
        },
        from: username.map(|u| User {
            id: 1,
            is_bot: false,
            username: Some(u.to_string()),
        }),
        text: text.map(std::string::ToString::to_string),
        caption: None,
        entities: None,
        reply_to_message: None,
        photo: None,
        voice: None,
        document: None,
    }
}

#[test]
fn reply_context_includes_original_text_and_sender() {
    let mut msg = make_msg("my reply", ChatType::Private);
    msg.reply_to_message = Some(Box::new(make_reply(
        Some("original message"),
        Some("alice"),
    )));
    let result = prepend_reply_context(&msg, "my reply".to_string());
    assert_eq!(
        result,
        "[Replying to message from @alice: \"original message\"]\n\nmy reply",
    );
}

#[test]
fn reply_context_uses_caption_when_no_text() {
    let mut reply = make_reply(None, Some("bob"));
    reply.caption = Some("photo caption".to_string());

    let mut msg = make_msg("nice pic", ChatType::Private);
    msg.reply_to_message = Some(Box::new(reply));

    let result = prepend_reply_context(&msg, "nice pic".to_string());
    assert!(result.contains("\"photo caption\""));
    assert!(result.ends_with("nice pic"));
}

#[test]
fn reply_context_falls_back_to_no_text() {
    let mut msg = make_msg("what was that?", ChatType::Private);
    msg.reply_to_message = Some(Box::new(make_reply(None, Some("carol"))));

    let result = prepend_reply_context(&msg, "what was that?".to_string());
    assert!(result.contains("[no text]"));
}

#[test]
fn reply_context_falls_back_to_unknown_sender() {
    let mut reply = make_reply(Some("hello"), None);
    reply.from = None;

    let mut msg = make_msg("hey", ChatType::Private);
    msg.reply_to_message = Some(Box::new(reply));

    let result = prepend_reply_context(&msg, "hey".to_string());
    assert!(result.contains("@unknown"));
}

#[test]
fn no_reply_returns_text_unchanged() {
    let msg = make_msg("hello", ChatType::Private);
    let result = prepend_reply_context(&msg, "hello".to_string());
    assert_eq!(result, "hello");
}

#[test]
fn reply_context_preserves_empty_reply_text() {
    let mut msg = make_msg("", ChatType::Private);
    msg.reply_to_message = Some(Box::new(make_reply(Some("original"), Some("dave"))));

    let result = prepend_reply_context(&msg, String::new());
    assert!(result.contains("\"original\""));
    assert!(result.ends_with("\n\n"));
}

#[test]
fn truncate_chars_short_string_unchanged() {
    assert_eq!(truncate_chars("hello", 10), "hello");
}

#[test]
fn truncate_chars_long_string_shortened_with_ellipsis() {
    let out = truncate_chars("abcdefghij", 5);
    assert_eq!(out, "abcd…");
    assert_eq!(out.chars().count(), 5);
}

#[test]
fn truncate_chars_handles_multibyte() {
    // Each of these is multi-byte in UTF-8 but one char.
    let out = truncate_chars("αβγδεζηθ", 4);
    assert_eq!(out.chars().count(), 4);
    assert!(out.ends_with('…'));
}

#[test]
fn agents_keyboard_has_stop_button_per_agent() {
    use std::time::Duration;
    let agents = vec![
        super::super::background::BackgroundAgentListEntry {
            id: 1,
            prompt_preview: "fix bug".into(),
            elapsed: Duration::from_secs(5),
            chat_id: "bg-1".into(),
        },
        super::super::background::BackgroundAgentListEntry {
            id: 2,
            prompt_preview: "write docs".into(),
            elapsed: Duration::from_secs(30),
            chat_id: "bg-2".into(),
        },
    ];
    let keyboard = build_agents_keyboard(&agents);
    assert_eq!(keyboard.inline_keyboard.len(), 2);
    for (i, row) in keyboard.inline_keyboard.iter().enumerate() {
        assert_eq!(row.len(), 1);
        let btn = &row[0];
        // The word "Stop" must be visible so the action is obvious.
        assert!(btn.text.contains("Stop"), "button text: {}", btn.text);
        assert!(btn.text.contains(&format!("#{}", agents[i].id)));
        assert_eq!(
            btn.callback_data.as_deref(),
            Some(format!("stop_agent:{}", agents[i].id).as_str()),
        );
    }
}

#[test]
fn agents_keyboard_empty_when_no_agents() {
    let keyboard = build_agents_keyboard(&[]);
    assert!(keyboard.inline_keyboard.is_empty());
}
