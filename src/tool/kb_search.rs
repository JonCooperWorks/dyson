// ===========================================================================
// KbSearchTool — full-text search over the knowledge base.
//
// Searches the FTS5 index for files under kb/ (raw sources and wiki
// articles).  Supports scoping to raw/, wiki/, or all KB files.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

use crate::error::DysonError;
use crate::tool::{Tool, ToolContext, ToolOutput};

pub struct KbSearchTool;

#[async_trait]
impl Tool for KbSearchTool {
    fn name(&self) -> &str {
        "kb_search"
    }

    fn description(&self) -> &str {
        "Search the knowledge base using full-text search. Returns matching articles \
         and snippets from kb/raw/ (source material) and kb/wiki/ (compiled articles). \
         Use the scope parameter to narrow results."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query — words or phrases to find in the knowledge base"
                },
                "scope": {
                    "type": "string",
                    "enum": ["all", "raw", "wiki"],
                    "description": "Search scope: 'all' (default), 'raw' (source material only), \
                                    or 'wiki' (compiled articles only)"
                }
            },
            "required": ["query"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let ws = ctx
            .workspace
            .as_ref()
            .ok_or_else(|| DysonError::tool("kb_search", "no workspace configured"))?;

        let query = input["query"].as_str().unwrap_or("").to_string();
        if query.is_empty() {
            return Ok(ToolOutput::error("query is required"));
        }

        let scope = input["scope"].as_str().unwrap_or("all");
        let prefix = match scope {
            "raw" => "kb/raw/",
            "wiki" => "kb/wiki/",
            _ => "kb/",
        };

        let ws = ws.read().await;
        let results = ws.memory_search(&query);

        // Filter to KB files matching the requested scope.
        let filtered: Vec<_> = results
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect();

        if filtered.is_empty() {
            return Ok(ToolOutput::success("No results found in the knowledge base."));
        }

        let mut output = String::new();
        for (key, snippet) in &filtered {
            output.push_str(&format!("### {key}\n{snippet}\n\n"));
        }
        output.push_str(&format!("({} result(s))", filtered.len()));

        Ok(ToolOutput::success(output))
    }
}
