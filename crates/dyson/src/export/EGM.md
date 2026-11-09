# Conversation Export (`export`)

## Overview
Serialises Dyson's internal `Message` conversations into external formats for training and archival. Currently ShareGPT, the de facto fine-tuning format.

## Interface
- `sharegpt` (pub) [mod.rs]
- `ShareGptConversation`, `ShareGptTurn` [sharegpt.rs:ShareGptConversation, sharegpt.rs:ShareGptTurn]
- `to_sharegpt(...)`, `to_sharegpt_with_feedback(...)`, `to_sharegpt_json(conversations) -> Result<String>` [sharegpt.rs:to_sharegpt, sharegpt.rs:to_sharegpt_with_feedback, sharegpt.rs:to_sharegpt_json]

## Invariants
- None beyond format mapping [sharegpt.rs:to_sharegpt]

## Side Effects
- None (pure conversion) [sharegpt.rs:to_sharegpt_json]

## Verification
- Run: `cargo test -p dyson` (from `dyson/`)
