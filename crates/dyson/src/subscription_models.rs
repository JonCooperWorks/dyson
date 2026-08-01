//! Account-visible native subscription model catalogues.
//!
//! These are shared by the immutable Swarm boot config and the runtime
//! configure path. Keeping both paths on one source of truth matters because
//! Cube rotations preserve `dyson.json` from the source snapshot.

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
