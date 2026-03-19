// ===========================================================================
// Dyson — streaming, composable AI agent loop in Rust.
//
// LEARNING OVERVIEW
//
// What this crate does:
//   Dyson is an AI agent framework.  The core idea: an LLM streams tool
//   calls in a loop until it has an answer.  Everything else — MCP servers,
//   skills, local tools — plugs into that loop through traits.
//
// Crate structure:
//
//   dyson (library crate)
//     ├── error       — DysonError enum, Result type alias
//     ├── message     — Message, Role, ContentBlock
//     ├── config      — Settings, loaders (dyson.toml, Claude, Cursor)
//     ├── tool        — Tool trait, ToolContext, ToolOutput, built-in tools
//     ├── skill       — Skill trait, BuiltinSkill
//     ├── sandbox     — Sandbox trait, DangerousNoSandbox
//     ├── llm         — LlmClient trait, Anthropic streaming client
//     ├── agent       — Agent loop, stream handler
//     └── ui          — Output trait, terminal renderer
//
// The binary crate (main.rs) wires everything together: parse CLI args,
// load config, create the agent, and run the interactive loop.
//
// Key design principles:
//   - Stream everything: text tokens go to the user as they arrive
//   - MCP is not special: it's a skill, skills are trait impls
//   - Settings are portable: parse any config format into one struct
//   - Sandbox gates everything: every tool call goes through a Sandbox trait
//   - Extensible by default: new providers, skills, tools — all traits
// ===========================================================================

pub mod agent;
pub mod config;
pub mod controller;
pub mod error;
pub mod llm;
pub mod message;
pub mod persistence;
pub mod sandbox;
pub mod secret;
pub mod skill;
pub mod tool;

// ---------------------------------------------------------------------------
// Re-exports for convenience
// ---------------------------------------------------------------------------

pub use error::{DysonError, Result};
