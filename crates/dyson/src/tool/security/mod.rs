// ===========================================================================
// Security tools — AST-aware security analysis for the security_engineer
// subagent swarm.
//
// Philosophy: no hardcoded vulnerability checks.  Instead, expose
// tree-sitter's Query API to the agent and let it write its own AST
// queries to trace patterns.  The system prompt includes p95 common
// patterns as guidance, but the agent constructs and executes queries
// itself — the agent IS the vulnerability scanner.
//
// Three tools:
//
//   ast_query               — execute tree-sitter S-expression queries
//                             against the codebase.  The agent writes
//                             queries to find whatever patterns it deems
//                             interesting.
//
//   attack_surface_analyzer — identifies all external-facing code: HTTP
//                             handlers, CLI arg parsing, file I/O, network
//                             calls, env var reads, database queries.
//
//   exploit_builder          — given a vulnerability finding + code context,
//                              generates a proof-of-concept exploit or
//                              nuclei template the LLM can use for
//                              verification.
//
// All three use the shared tree-sitter infrastructure from `tool::ast`,
// so they support the same 20 languages and respect the same file-size
// and file-count limits.
// ===========================================================================

pub mod ast_query;
pub mod attack_surface_analyzer;
pub mod exploit_builder;

pub use ast_query::AstQueryTool;
pub use attack_surface_analyzer::AttackSurfaceAnalyzerTool;
pub use exploit_builder::ExploitBuilderTool;
