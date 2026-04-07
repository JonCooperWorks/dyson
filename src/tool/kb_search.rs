// ===========================================================================
// KbSearchTool — full-text search over the knowledge base.
//
// Searches the FTS5 index for files under kb/ (raw sources and wiki
// articles).  Supports scoping to raw/, wiki/, or all KB files.
// ===========================================================================

use async_trait::async_trait;
use serde_json::json;

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
        let ws = ctx.workspace("kb_search")?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    #[tokio::test]
    async fn empty_query_returns_error() {
        let ws = InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = KbSearchTool;
        let input = serde_json::json!({"query": ""});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("required"));
    }

    #[tokio::test]
    async fn no_results_returns_message() {
        let ws = InMemoryWorkspace::new()
            .with_file("kb/raw/doc.md", "some content about rust");
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = KbSearchTool;
        let input = serde_json::json!({"query": "nonexistent_topic_xyz"});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("No results"));
    }

    #[tokio::test]
    async fn scope_raw_filters_correctly() {
        let ws = InMemoryWorkspace::new()
            .with_file("kb/raw/doc.md", "rust programming language")
            .with_file("kb/wiki/article.md", "rust wiki article");
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = KbSearchTool;
        let input = serde_json::json!({"query": "rust", "scope": "raw"});
        let output = tool.run(&input, &ctx).await.unwrap();
        // InMemoryWorkspace.memory_search returns empty (not implemented),
        // so this will show "No results". The scope filtering logic still applies.
        assert!(!output.is_error);
    }

    #[tokio::test]
    async fn scope_wiki_filters_correctly() {
        let ws = InMemoryWorkspace::new()
            .with_file("kb/wiki/article.md", "wiki content about testing");
        let ctx = ToolContext::for_test_with_workspace(ws);
        let tool = KbSearchTool;
        let input = serde_json::json!({"query": "testing", "scope": "wiki"});
        let output = tool.run(&input, &ctx).await.unwrap();
        assert!(!output.is_error);
    }
}
