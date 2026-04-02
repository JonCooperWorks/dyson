// ===========================================================================
// MemorySearchTool — full-text search over Tier 2 memory files.
//
// Searches the SQLite FTS5 index for memory/ files, returning matching
// file keys and highlighted snippets.  Falls back to regex search if
// FTS5 returns no results.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct MemorySearchTool;

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search across memory files using full-text search. Use this to find information \
         stored in memory/notes/ and other memory/ files. Returns matching file names and \
         relevant snippets."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query — words or phrases to find in memory files"
                }
            },
            "required": ["query"]
        })
    }

    async fn run(&self, input: serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx
            .workspace
            .as_ref()
            .ok_or_else(|| DysonError::tool("memory_search", "no workspace configured"))?;

        let query = input["query"].as_str().unwrap_or("").to_string();

        if query.is_empty() {
            return Ok(ToolOutput::error("query is required"));
        }

        let ws = ws.read().await;
        let results = ws.memory_search(&query);

        if results.is_empty() {
            return Ok(ToolOutput::success("No results found."));
        }

        let mut output = String::new();
        for (key, snippet) in &results {
            output.push_str(&format!("### {key}\n{snippet}\n\n"));
        }
        output.push_str(&format!("({} result(s))", results.len()));

        Ok(ToolOutput::success(output))
    }
}
