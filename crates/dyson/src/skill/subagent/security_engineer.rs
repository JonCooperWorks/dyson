// ===========================================================================
// Security engineer orchestrator — composed from OrchestratorTool.
//
// This module defines the OrchestratorConfig for the security_engineer role.
// The actual OrchestratorTool implementation is in orchestrator.rs.
// ===========================================================================

use super::orchestrator::OrchestratorConfig;
use crate::message::ArtefactKind;

const DIRECT_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "search_files",
    "list_files",
    "ast_describe",
    "ast_query",
    "attack_surface_analyzer",
    "exploit_builder",
    "taint_trace",
    "dependency_scan",
];

/// Build the OrchestratorConfig for the security_engineer role.
///
/// The security engineer gets AST-aware tools (ast_query,
/// attack_surface_analyzer, exploit_builder) plus standard read tools,
/// and can dispatch inner subagents (planner, researcher, coder, verifier)
/// in parallel at depth 2.
pub fn security_engineer_config() -> OrchestratorConfig {
    OrchestratorConfig {
        name: "security_engineer",
        description: "Spawns a security engineer agent that performs comprehensive security \
             analysis using AST-aware tools.  Can write custom tree-sitter queries \
             to trace vulnerability patterns, map attack surfaces, generate exploit \
             PoCs, and dispatch subagents (researcher, coder, verifier) in parallel.  \
             Use for security reviews, vulnerability assessments, and validating \
             security-sensitive changes.",
        system_prompt: include_str!("prompts/security_engineer.md"),
        direct_tool_names: DIRECT_TOOLS,
        // Capped at 80 to bound *context* consumption, not just turn count:
        // every tool result stays in the transcript, so an unbounded iteration
        // budget pushes the subagent past the ~200k-token window and yields
        // a 400 error (or, worse, garbage output from a truncated request).
        // 80 turns is enough for a focused review of one module/crate; broader
        // reviews should be split across multiple invocations with scoped paths.
        max_iterations: 80,
        max_tokens: 8192,
        injects_protocol: Some(include_str!("prompts/security_engineer_protocol.md")),
        // Only the security_engineer wants lang/framework vuln sheets;
        // other orchestrators leave this false.
        inject_cheatsheets: true,
        // The final report is a first-class artefact: the web UI
        // renders it in the Artefacts tab instead of as a wall of
        // chat text.  Full content also flows to the parent LLM via
        // ToolOutput.content as before.
        emit_artefact: Some(ArtefactKind::SecurityReview),
    }
}
