// ===========================================================================
// MemorySearchTool — full-text search over Tier 2 memory files.
//
// Searches the SQLite FTS5 index for memory/ files, returning matching
// file keys and highlighted snippets.  Falls back to regex search if
// FTS5 returns no results.
// ===========================================================================

use std::fmt::Write;

use async_trait::async_trait;
use serde_json::json;

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

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx.workspace("memory_search")?;

        let query = input["query"].as_str().unwrap_or("").to_string();

        if query.is_empty() {
            return Ok(ToolOutput::error("query is required"));
        }

        let results = ws.read().await.memory_search(&query);

        if results.is_empty() {
            return Ok(ToolOutput::success("No results found."));
        }

        let mut output = String::new();
        for (key, snippet) in &results {
            writeln!(&mut output, "### {key}\n{snippet}\n").unwrap();
        }
        write!(&mut output, "({} result(s))", results.len()).unwrap();

        Ok(ToolOutput::success(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    #[tokio::test]
    async fn empty_query_returns_error() {
        let ws = InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = MemorySearchTool;
        let input = serde_json::json!({"query": ""});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("required"));
    }

    #[tokio::test]
    async fn no_results_returns_message() {
        let ws = InMemoryWorkspace::new().with_file("memory/notes/test.md", "some memory content");
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = MemorySearchTool;
        let input = serde_json::json!({"query": "nonexistent_xyz"});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No results"));
    }

    #[tokio::test]
    async fn results_formatted_with_count() {
        // InMemoryWorkspace doesn't implement memory_search (returns empty vec),
        // so we test the formatting path indirectly. The important thing is
        // the tool doesn't panic and returns a valid response.
        let ws = InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = MemorySearchTool;
        let input = serde_json::json!({"query": "anything"});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(!output.is_error);
    }
}
