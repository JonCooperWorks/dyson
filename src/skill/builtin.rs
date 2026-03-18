// ===========================================================================
// BuiltinSkill — Dyson's built-in tool suite.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Wraps Dyson's built-in tools (bash, and future read_file/write_file/
//   edit_file) into a Skill implementation.  This is the default skill
//   that's always loaded unless explicitly disabled.
//
// Why wrap tools in a skill?
//   The agent loop only interacts with skills — it doesn't know about
//   individual tools directly.  By wrapping builtins in a Skill, they
//   plug into the same lifecycle (on_load, on_unload, after_tool) and
//   discovery mechanism as MCP skills and local skills.  No special-casing.
//
// Adding new built-in tools:
//   1. Create the tool in tool/ (e.g., tool/read_file.rs)
//   2. Add it to the `tools` vec in BuiltinSkill::new()
//   3. Done — the agent discovers it automatically via the Skill trait
// ===========================================================================

use std::sync::Arc;

use async_trait::async_trait;

use crate::skill::Skill;
use crate::tool::Tool;
use crate::tool::bash::BashTool;

// ---------------------------------------------------------------------------
// BuiltinSkill
// ---------------------------------------------------------------------------

/// Skill that provides Dyson's built-in tools.
///
/// Phase 1: just bash.  Future phases add read_file, write_file, edit_file,
/// web_search, and more.  The system prompt fragment describes what tools
/// are available and gives the LLM usage guidance.
pub struct BuiltinSkill {
    /// The built-in tools, stored as Arc for shared ownership with the agent.
    tools: Vec<Arc<dyn Tool>>,

    /// System prompt describing the available tools.
    ///
    /// Generated at construction time based on which tools are enabled.
    system_prompt: String,
}

impl BuiltinSkill {
    /// Create a new BuiltinSkill with all default tools.
    pub fn new() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(BashTool::default())];

        // Build the system prompt dynamically from the loaded tools.
        let tool_list: Vec<String> = tools
            .iter()
            .map(|t| format!("- **{}**: {}", t.name(), t.description()))
            .collect();

        let system_prompt = format!(
            "You have access to the following built-in tools:\n\n{}\n\n\
             Use these tools to help answer questions and complete tasks. \
             When running commands, prefer concise output. \
             Check command results before proceeding to the next step.",
            tool_list.join("\n")
        );

        Self {
            tools,
            system_prompt,
        }
    }
}

impl Default for BuiltinSkill {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Skill implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Skill for BuiltinSkill {
    fn name(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    fn system_prompt(&self) -> Option<&str> {
        Some(&self.system_prompt)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_bash_tool() {
        let skill = BuiltinSkill::new();
        let tools = skill.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "bash");
    }

    #[test]
    fn has_system_prompt() {
        let skill = BuiltinSkill::new();
        let prompt = skill.system_prompt().unwrap();
        assert!(prompt.contains("bash"));
        assert!(!prompt.is_empty());
    }

    #[test]
    fn skill_name() {
        let skill = BuiltinSkill::new();
        assert_eq!(skill.name(), "builtin");
    }
}
