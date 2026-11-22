//! Account-visible native subscription model catalogues.
//!
//! `dyson swarm` writes these into every boot's dyson.json.

/// Models returned by Codex's authenticated `model/list` response.
pub const CHATGPT: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex-spark",
];

/// Canonical model IDs advertised by Claude Code to the connected Max account.
pub const CLAUDE: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];
