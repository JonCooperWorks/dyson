# Telegram Controller (`controller/telegram`)

## Overview
Telegram Bot API front end for the Dyson agent. Receives updates by long polling or webhook, maintains per-chat agent entries, routes commands, callbacks (inline model/stop buttons, emoji reactions) and messages, runs agent turns, and streams responses back by editing messages with Telegram-flavoured HTML.

## Interface
- `TelegramController` - implements `Controller` [mod.rs:TelegramController]
- `enqueue_webhook_update(update)` - hands a webhook `Update` to the running receiver [mod.rs:enqueue_webhook_update]
- `BotApi` - `new`, `new_with_base`, `get_me`, `get_updates`, `send_message`, `send_message_html`, `send_message_with_keyboard`, `edit_message_text`, `edit_message_text_plain`, `send_document`, `answer_callback_query`, `send_typing`, `download_file` [api.rs:BotApi]
- `TelegramOutput` - `Output` impl that edits a live message and sends files/artefacts [output.rs:TelegramOutput]
- `types` (pub): `ChatId`, `MessageId`, `Update`, `Message`, `ChatType`, `Chat`, `User`, `MessageEntity`, `PhotoSize`, `Voice`, `Document`, `CallbackQuery`, `File`, `InlineKeyboardMarkup`, `InlineKeyboardButton`, `MessageReactionUpdated`, `ReactionType`, `ApiResponse<T>` [types.rs:Update]
- Formatting: `split_for_telegram`, `split_for_telegram_at`, `markdown_to_telegram_html`, `escape_html`, `format_logs_for_telegram`, `strip_bot_mention`, `is_public_command` [formatting.rs:markdown_to_telegram_html]
- Authorization: `is_operator` (pub), `should_reject`, `should_reject_group_command`, `is_directed_group_msg`, `is_bot_mentioned`, `is_reply_to_bot` [messages.rs:is_operator]
- `TelegramControllerConfig`, `TelegramProxyConfig { base_url, file_base_url, bearer }`, `TelegramMode { Polling (default), Webhook }` [config.rs:TelegramControllerConfig, config.rs:TelegramMode]
- Internals: `handle_per_chat_command`, `handle_instant_command`, `build_model_keyboard` [commands.rs]; `handle_callback_query`, `handle_reaction` [callbacks.rs]; `get_or_create_entry`, `rebuild_agents_on_reload` [chats.rs]; `run_telegram_turn`, `run_agent_for_message` [turns.rs]; `extract_attachments` [attachments.rs]

## Invariants
- `allowed_chat_ids` doubles as the operator allowlist: `is_operator` requires the sender id to be in it [messages.rs:is_operator]
- Private chats not in a non-empty allowlist are rejected [messages.rs:should_reject]; in groups, non-public slash commands from non-operators are rejected [messages.rs:should_reject_group_command]
- Messages are split at `MAX_MESSAGE_LEN = 4000` chars [mod.rs:MAX_MESSAGE_LEN, formatting.rs:split_for_telegram]
- Message edits are throttled to `EDIT_INTERVAL_MS = 500` [mod.rs:EDIT_INTERVAL_MS]
- At most `MAX_CHAT_ENTRIES = 200` chat entries and `MAX_IN_FLIGHT_PER_CHAT = 3` concurrent turns per chat (semaphore) [mod.rs:MAX_CHAT_ENTRIES, mod.rs:MAX_IN_FLIGHT_PER_CHAT, chats.rs:get_or_create_entry]
- `enqueue_webhook_update` errors when no receiver is installed or it is closed [mod.rs:enqueue_webhook_update]

## Side Effects
- HTTPS calls to the Telegram Bot API (or a configured proxy) via the shared `http::client()` [api.rs:BotApi]
- Downloads Telegram media for attachments [attachments.rs:extract_attachments, api.rs:BotApi::download_file]
- Writes artefacts to a temp file in `std::env::temp_dir()`, uploads via `sendDocument`, then deletes it [output.rs:TelegramOutput]
- Spawns a typing-indicator task per output [output.rs:TelegramOutput] and per-message turn tasks [mod.rs:TelegramController]
- Process-global webhook sender slot `TELEGRAM_WEBHOOK_SENDER` [mod.rs:TELEGRAM_WEBHOOK_SENDER]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
