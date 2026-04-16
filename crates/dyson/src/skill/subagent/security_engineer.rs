// ===========================================================================
// Security engineer orchestrator — composed from OrchestratorTool.
//
// This module defines the OrchestratorConfig for the security_engineer role.
// The actual OrchestratorTool implementation is in orchestrator.rs.
// ===========================================================================

use super::orchestrator::OrchestratorConfig;

/// Build the OrchestratorConfig for the security_engineer role.
///
/// The security engineer gets AST-aware tools (ast_query,
/// attack_surface_analyzer, exploit_builder) plus standard read tools,
/// and can dispatch inner subagents (planner, researcher, coder, verifier)
/// in parallel at depth 2.
pub fn security_engineer_config() -> OrchestratorConfig {
    OrchestratorConfig {
        name: "security_engineer".into(),
        description: "Spawns a security engineer agent that performs comprehensive security \
             analysis using AST-aware tools.  Can write custom tree-sitter queries \
             to trace vulnerability patterns, map attack surfaces, generate exploit \
             PoCs, and dispatch subagents (researcher, coder, verifier) in parallel.  \
             Use for security reviews, vulnerability assessments, and validating \
             security-sensitive changes."
            .into(),
        system_prompt: include_str!("prompts/security_engineer.md").into(),
        direct_tool_names: vec![
            "bash".into(),
            "read_file".into(),
            "search_files".into(),
            "list_files".into(),
            "ast_query".into(),
            "attack_surface_analyzer".into(),
            "exploit_builder".into(),
        ],
        max_iterations: 40,
        max_tokens: 8192,
        injects_protocol: Some(
            include_str!("prompts/security_engineer_protocol.md").into(),
        ),
    }
}
