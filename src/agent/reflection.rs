// ===========================================================================
// Reflection — Dream implementations for memory and self-improvement.
//
// This module contains the three built-in dreams:
//
//   1. LearningSynthesisDream  — single LLM call to merge conversation
//      learnings into MEMORY.md.  Fires after compaction.
//
//   2. MemoryMaintenanceDream  — mini agent loop with workspace tools
//      to update MEMORY.md, USER.md, and overflow notes.  Fires every
//      N user turns.
//
//   3. SelfImprovementDream    — mini agent loop that creates skills
//      or exports training data.  Fires every 2N turns.
//
// All three implement the Dream trait (see dream.rs) and run as
// fire-and-forget background tasks.  Nothing from these calls enters
// the main conversation history.
//
// See docs/dreaming.md for the full design document.
// ===========================================================================

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message};
use crate::tool::{Tool, ToolContext};

use super::dream::{Dream, DreamContext, DreamOutcome, DreamTrigger};
use super::silent_output::SilentOutput;
use super::stream_handler;

// ---------------------------------------------------------------------------
// Conversation summariser — shared by all dreams
// ---------------------------------------------------------------------------

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from max_bytes to find a char boundary.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Summarize a conversation for dream consumption.
///
/// Instead of sending the full message history (which could be huge),
/// build a condensed representation: user goals, tools used, outcomes.
pub(super) fn summarize_for_reflection(messages: &[Message]) -> String {
    let mut summary = String::from(
        "Review this conversation and decide whether to create/improve a skill \
         or export training data.  Here is what happened:\n\n",
    );

    let mut tool_call_count = 0;
    let mut tool_error_count = 0;
    let mut tools_used_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tools_used: Vec<String> = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let role = match msg.role {
                        crate::message::Role::User => "User",
                        crate::message::Role::Assistant => "Assistant",
                    };
                    let truncated = if text.len() > 500 {
                        format!("{}...[truncated]", truncate_str(text, 500))
                    } else {
                        text.clone()
                    };
                    summary.push_str(&format!("{role}: {truncated}\n\n"));
                }
                ContentBlock::ToolUse { name, .. } => {
                    tool_call_count += 1;
                    if tools_used_set.insert(name.clone()) {
                        tools_used.push(name.clone());
                    }
                    summary.push_str(&format!("[Tool call: {name}]\n"));
                }
                ContentBlock::ToolResult {
                    is_error, content, ..
                } => {
                    if *is_error {
                        tool_error_count += 1;
                        summary.push_str(&format!(
                            "[Tool error: {}]\n",
                            truncate_str(content, 200)
                        ));
                    } else {
                        let truncated = if content.len() > 200 {
                            format!("{}...", truncate_str(content, 200))
                        } else {
                            content.clone()
                        };
                        summary.push_str(&format!("[Tool result: {truncated}]\n"));
                    }
                }
                ContentBlock::Image { media_type, .. } => {
                    summary.push_str(&format!("[Image: {media_type}]\n"));
                }
            }
        }
    }

    summary.push_str(&format!(
        "\n---\nStats: {tool_call_count} tool calls ({tool_error_count} errors), \
         tools used: [{}], {} messages total.",
        tools_used.join(", "),
        messages.len(),
    ));

    summary
}

// ===========================================================================
// 1. LearningSynthesisDream
// ===========================================================================

/// Consolidates conversation learnings into MEMORY.md via a single LLM call.
///
/// This is the lightest dream — no tools, just one prompt asking the LLM
/// to merge new information into the existing memory file.  Fires after
/// compaction so learnings are captured before the conversation is condensed.
pub struct LearningSynthesisDream;

#[async_trait]
impl Dream for LearningSynthesisDream {
    fn name(&self) -> &str {
        "learning-synthesis"
    }

    fn trigger(&self) -> DreamTrigger {
        DreamTrigger::AfterCompaction
    }

    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome> {
        let workspace = match ctx.tool_context.workspace {
            Some(ref ws) => Arc::clone(ws),
            None => {
                return Ok(DreamOutcome {
                    dream_name: self.name().to_string(),
                    actions_taken: 0,
                    duration: std::time::Duration::ZERO,
                    artifacts: vec!["skipped: no workspace".to_string()],
                });
            }
        };

        let start = std::time::Instant::now();

        // Access the LLM client through the rate-limited handle.
        // This checks the shared rate limiter at Background priority —
        // if the window is too full, the dream yields to user-facing calls.
        let client = ctx.client.access()?;

        synthesize_to_workspace(&**client, &ctx.config, &ctx.conversation_summary, &workspace)
            .await?;

        let new_len = {
            let ws = workspace.read().await;
            ws.get("MEMORY.md").map(|c| c.len()).unwrap_or(0)
        };

        Ok(DreamOutcome {
            dream_name: self.name().to_string(),
            actions_taken: 1,
            duration: start.elapsed(),
            artifacts: vec![format!("updated MEMORY.md ({new_len} chars)")],
        })
    }
}

// ===========================================================================
// 2. MemoryMaintenanceDream
// ===========================================================================

/// Reviews conversation and updates workspace memory files using tools.
///
/// Unlike LearningSynthesisDream (which does a single write), this dream
/// runs a mini agent loop with workspace_view, workspace_update,
/// workspace_search, and memory_search tools.  It can read existing
/// files, make targeted updates, and use overflow storage.
pub struct MemoryMaintenanceDream {
    /// Fire every N user turns.
    nudge_interval: usize,
}

impl MemoryMaintenanceDream {
    pub fn new(nudge_interval: usize) -> Self {
        Self { nudge_interval }
    }
}

#[async_trait]
impl Dream for MemoryMaintenanceDream {
    fn name(&self) -> &str {
        "memory-maintenance"
    }

    fn trigger(&self) -> DreamTrigger {
        DreamTrigger::EveryNTurns(self.nudge_interval)
    }

    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome> {
        let start = std::time::Instant::now();

        // Build system prompt (reads workspace stats).
        let memory_system = build_memory_system_prompt(&ctx.tool_context).await;

        // Create the four memory tools as standalone instances.
        let memory_tool_instances: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tool::workspace_view::WorkspaceViewTool),
            Arc::new(crate::tool::workspace_update::WorkspaceUpdateTool),
            Arc::new(crate::tool::workspace_search::WorkspaceSearchTool),
            Arc::new(crate::tool::memory_search::MemorySearchTool),
        ];

        let memory_tools: Vec<ToolDefinition> = memory_tool_instances
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                agent_only: false,
            })
            .collect();

        let tool_map: HashMap<String, Arc<dyn Tool>> = memory_tool_instances
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();

        let mut messages = vec![Message::user(&ctx.conversation_summary)];
        let mut actions_taken = 0usize;

        for _iteration in 0..5u8 {
            // Access the LLM client through the rate-limited handle.
            // If the window is too full for Background priority, stop early.
            let client = match ctx.client.access() {
                Ok(guard) => guard,
                Err(e) => {
                    tracing::info!(error = %e, "memory maintenance: rate limited, stopping early");
                    break;
                }
            };

            let response = match client
                .stream(&messages, &memory_system, "", &memory_tools, &ctx.config)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "memory maintenance LLM call failed");
                    break;
                }
            };

            let mut silent = SilentOutput;
            let (assistant_msg, tool_calls, _tokens) =
                stream_handler::process_stream(response.stream, &mut silent).await?;

            messages.push(assistant_msg);

            if tool_calls.is_empty() {
                break;
            }

            for call in &tool_calls {
                let tool = match tool_map.get(&call.name) {
                    Some(t) => Arc::clone(t),
                    None => {
                        tracing::warn!(tool = call.name.as_str(), "memory maintenance: unknown tool");
                        messages.push(Message::tool_result(
                            &call.id,
                            &format!("unknown tool '{}'", call.name),
                            true,
                        ));
                        continue;
                    }
                };

                let result = tool.run(&call.input, &ctx.tool_context).await;
                let tool_result_msg = match result {
                    Ok(ref output) => {
                        actions_taken += 1;
                        tracing::info!(tool = call.name.as_str(), "memory maintenance: tool ok");
                        Message::tool_result(&call.id, &output.content, output.is_error)
                    }
                    Err(ref e) => {
                        tracing::warn!(
                            tool = call.name.as_str(),
                            error = %e,
                            "memory maintenance: tool failed"
                        );
                        Message::tool_result(&call.id, &e.to_string(), true)
                    }
                };
                messages.push(tool_result_msg);
            }
        }

        Ok(DreamOutcome {
            dream_name: self.name().to_string(),
            actions_taken,
            duration: start.elapsed(),
            artifacts: vec![format!("{actions_taken} workspace tool calls executed")],
        })
    }
}

// ---------------------------------------------------------------------------
// Learning synthesis — testable workhorse for LearningSynthesisDream
// ---------------------------------------------------------------------------

/// Summarise a conversation and merge the result into the workspace's
/// MEMORY.md.  Separated from the Dream impl so it can be tested with
/// a mock LLM client.
pub(super) async fn synthesize_to_workspace(
    client: &dyn LlmClient,
    config: &CompletionConfig,
    conversation_summary: &str,
    workspace: &Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
) -> Result<()> {
    let current_memory = {
        let ws = workspace.read().await;
        ws.get("MEMORY.md").unwrap_or_default()
    };

    let system = "\
        You are a memory maintenance engine.  You will receive the current \
        contents of MEMORY.md and a summary of a conversation that is about \
        to be cleared.  Your job is to produce an updated MEMORY.md that \
        incorporates any important new information from the conversation.\n\n\
        Rules:\n\
        - Output ONLY the new file contents, no commentary or markdown fences.\n\
        - Synthesise — merge new information into the existing text rather \
          than appending.\n\
        - Be concise.  Keep the file under 4000 characters.\n\
        - Omit trivial exchanges (greetings, simple lookups).\n\
        - Preserve existing information unless it's been superseded.\n\
        - If there is nothing worth persisting, output the existing file \
          unchanged.";

    let user_message = format!(
        "## Current MEMORY.md\n\n{current_memory}\n\n\
         ## Conversation summary\n\n{conversation_summary}"
    );

    let messages = vec![Message::user(&user_message)];
    let empty_tools: Vec<ToolDefinition> = Vec::new();

    let response = client
        .stream(&messages, system, "", &empty_tools, config)
        .await?;

    let mut silent = SilentOutput;
    let (assistant_msg, _, _) =
        stream_handler::process_stream(response.stream, &mut silent).await?;

    let mut new_memory = String::new();
    for block in &assistant_msg.content {
        if let ContentBlock::Text { text } = block {
            if !new_memory.is_empty() {
                new_memory.push('\n');
            }
            new_memory.push_str(text);
        }
    }

    if new_memory.trim().is_empty() {
        tracing::info!("learning synthesis produced empty output — skipping write");
        return Ok(());
    }

    {
        let mut ws = workspace.write().await;
        ws.set("MEMORY.md", new_memory.trim());
        ws.save()?;
    }

    tracing::info!(
        new_len = new_memory.len(),
        "MEMORY.md updated by learning synthesis"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// System prompt builders — public for testing
// ---------------------------------------------------------------------------

/// Build the system prompt for memory maintenance.
pub(super) async fn build_memory_system_prompt(ctx: &ToolContext) -> String {
    let (memory_usage, memory_limit, user_usage, user_limit) =
        if let Some(ref ws) = ctx.workspace {
            let ws = ws.read().await;
            let mu = ws.get("MEMORY.md").map(|c| c.chars().count()).unwrap_or(0);
            let ml = ws.char_limit("MEMORY.md").unwrap_or(0);
            let uu = ws.get("USER.md").map(|c| c.chars().count()).unwrap_or(0);
            let ul = ws.char_limit("USER.md").unwrap_or(0);
            (mu, ml, uu, ul)
        } else {
            (0, 0, 0, 0)
        };

    format!(
        "You are a memory maintenance engine for an AI agent.  Your job is to review \
         a conversation that just happened and persist important information.\n\n\
         You have workspace tools to view and update the agent's memory files.\n\n\
         ## Files and limits\n\
         - **MEMORY.md** ({memory_usage}/{memory_limit} chars): Agent's curated long-term \
           memory.  Store key facts, decisions, patterns, and lessons learned.\n\
         - **USER.md** ({user_usage}/{user_limit} chars): What the agent knows about the \
           user — preferences, workflow, communication style.\n\
         - **memory/notes/*.md**: Overflow storage for details that don't fit in \
           MEMORY.md.  Searchable via memory_search.\n\n\
         ## What to persist\n\
         - Important facts, decisions, or conclusions from the conversation\n\
         - User preferences or workflow patterns you observed\n\
         - Technical details that would be useful in future sessions\n\
         - Lessons learned from errors or unexpected results\n\n\
         ## What NOT to persist\n\
         - Trivial or one-off exchanges (greetings, simple questions)\n\
         - Information already in the memory files\n\
         - Raw tool output — summarize the insight, not the data\n\n\
         First use workspace_view to read the current memory files.  Then \
         synthesize new information into the existing content — rewrite the \
         file as one cohesive, concise document rather than appending.  Use \
         workspace_update with mode \"set\" to replace the full file.  If a \
         file is near its limit, tighten prose or move lower-priority details \
         to memory/notes/.\n\n\
         Doing nothing is fine if there's nothing worth persisting."
    )
}

// ===========================================================================
// 3. SelfImprovementDream
// ===========================================================================

/// Reviews conversation and creates skills or exports training data.
///
/// Runs a mini agent loop with skill_create and export_conversation tools.
/// Fires at half the frequency of memory maintenance (every 2N turns)
/// and only after a minimum number of turns have elapsed.
pub struct SelfImprovementDream {
    /// Fire every 2 * nudge_interval turns, with a minimum turn gate.
    nudge_interval: usize,
}

impl SelfImprovementDream {
    pub fn new(nudge_interval: usize) -> Self {
        Self { nudge_interval }
    }
}

#[async_trait]
impl Dream for SelfImprovementDream {
    fn name(&self) -> &str {
        "self-improvement"
    }

    fn trigger(&self) -> DreamTrigger {
        // Fires at 2x the nudge interval — half as often as memory maintenance.
        DreamTrigger::EveryNTurns(self.nudge_interval * 2)
    }

    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome> {
        // Extra gate: don't fire until at least nudge_interval turns have passed.
        // This prevents self-improvement from running on very short conversations.
        if ctx.turn_count <= self.nudge_interval {
            return Ok(DreamOutcome {
                dream_name: self.name().to_string(),
                actions_taken: 0,
                duration: std::time::Duration::ZERO,
                artifacts: vec!["skipped: too few turns".to_string()],
            });
        }

        let start = std::time::Instant::now();

        let reflection_system = build_reflection_system_prompt(&ctx.tool_context).await;

        // Build the two reflection tools as standalone instances.
        let skill_create_tool: Arc<dyn Tool> =
            Arc::new(crate::tool::skill_create::SkillCreateTool);
        let export_tool: Arc<dyn Tool> =
            Arc::new(crate::tool::export_conversation::ExportConversationTool);

        let tool_map: HashMap<String, Arc<dyn Tool>> = HashMap::from([
            (
                skill_create_tool.name().to_string(),
                Arc::clone(&skill_create_tool),
            ),
            (export_tool.name().to_string(), Arc::clone(&export_tool)),
        ]);

        let reflection_tools: Vec<ToolDefinition> = [&skill_create_tool, &export_tool]
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                agent_only: false,
            })
            .collect();

        let mut messages = vec![Message::user(&ctx.conversation_summary)];
        let mut actions_taken = 0usize;
        let mut artifacts = Vec::new();

        for iteration in 0..3u8 {
            tracing::info!(iteration, "self-improvement LLM call");

            // Access the LLM client through the rate-limited handle.
            let client = match ctx.client.access() {
                Ok(guard) => guard,
                Err(e) => {
                    tracing::info!(error = %e, "self-improvement: rate limited, stopping early");
                    break;
                }
            };

            let response = match client
                .stream(
                    &messages,
                    &reflection_system,
                    "",
                    &reflection_tools,
                    &ctx.config,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "self-improvement LLM call failed");
                    break;
                }
            };

            let mut silent = SilentOutput;
            let (assistant_msg, tool_calls, _tokens) =
                stream_handler::process_stream(response.stream, &mut silent).await?;

            for block in &assistant_msg.content {
                if let ContentBlock::Text { text } = block
                    && !text.trim().is_empty()
                {
                    tracing::info!(reasoning = text.as_str(), "self-improvement reasoning");
                }
            }

            messages.push(assistant_msg);

            if tool_calls.is_empty() {
                tracing::info!("self-improvement: model decided no action needed");
                break;
            }

            for call in &tool_calls {
                tracing::info!(tool = call.name.as_str(), "self-improvement executing tool");

                let tool = match tool_map.get(&call.name) {
                    Some(t) => Arc::clone(t),
                    None => {
                        tracing::warn!(tool = call.name.as_str(), "self-improvement: unknown tool");
                        messages.push(Message::tool_result(
                            &call.id,
                            &format!("unknown tool '{}'", call.name),
                            true,
                        ));
                        continue;
                    }
                };

                let tool_start = std::time::Instant::now();
                let result = tool.run(&call.input, &ctx.tool_context).await;
                let tool_ms = tool_start.elapsed().as_millis();

                let tool_result_msg = match result {
                    Ok(ref output) => {
                        actions_taken += 1;
                        artifacts.push(format!("{}(ok, {}ms)", call.name, tool_ms));
                        tracing::info!(
                            tool = call.name.as_str(),
                            duration_ms = tool_ms,
                            "self-improvement tool ok"
                        );
                        Message::tool_result(&call.id, &output.content, output.is_error)
                    }
                    Err(ref e) => {
                        tracing::warn!(
                            tool = call.name.as_str(),
                            duration_ms = tool_ms,
                            error = %e,
                            "self-improvement tool failed"
                        );
                        Message::tool_result(&call.id, &e.to_string(), true)
                    }
                };
                messages.push(tool_result_msg);
            }
        }

        // Save reflection log to workspace.
        save_reflection_log(
            &ctx.tool_context,
            &reflection_system,
            &messages,
            actions_taken,
            ctx.turn_count,
        )
        .await;

        Ok(DreamOutcome {
            dream_name: self.name().to_string(),
            actions_taken,
            duration: start.elapsed(),
            artifacts,
        })
    }
}

/// Build the system prompt for self-improvement reflection.
pub(super) async fn build_reflection_system_prompt(ctx: &ToolContext) -> String {
    let existing_skills = if let Some(ref ws) = ctx.workspace {
        let ws = ws.read().await;
        let skill_dirs = ws.skill_dirs();
        if skill_dirs.is_empty() {
            "No skills exist yet.".to_string()
        } else {
            let names: Vec<String> = skill_dirs
                .iter()
                .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
                .collect();
            format!("Existing skills: {}", names.join(", "))
        }
    } else {
        "No workspace configured.".to_string()
    };

    format!(
        "You are a self-improvement engine for an AI agent.  Your job is to review \
         a conversation that just happened and decide whether to take action.\n\n\
         You have two tools:\n\
         - **skill_create**: Create or improve a SKILL.md file in the workspace.  \
           Skills are system prompt fragments that auto-load on startup, teaching \
           the agent reusable procedures.\n\
         - **export_conversation**: Export the conversation as ShareGPT-format \
           training data for fine-tuning tool-calling models.\n\n\
         ## When to create a skill\n\
         - The agent solved a complex, multi-step task that it might encounter again\n\
         - The agent discovered a non-obvious procedure or debugging pattern\n\
         - The agent found a domain-specific workflow worth encoding\n\n\
         ## When to improve a skill\n\
         - An existing skill's instructions were insufficient and the agent had to \
           improvise — capture what worked\n\
         - The agent found a better approach than what the skill describes\n\n\
         ## When to export\n\
         - The conversation contains substantial, high-quality tool use (3+ tool \
           calls with successful outcomes)\n\
         - The conversation demonstrates a complete problem-solving trajectory\n\n\
         ## When to do nothing\n\
         - The conversation was trivial (simple Q&A, one-step tasks)\n\
         - A matching skill already exists and doesn't need improvement\n\
         - The conversation was mostly errors or failed attempts\n\n\
         Doing nothing is the right choice most of the time.  Only act when there's \
         genuine value in persisting knowledge or exporting data.\n\n\
         {existing_skills}"
    )
}

/// Save a reflection exchange to the workspace for later inspection.
async fn save_reflection_log(
    tool_context: &ToolContext,
    system_prompt: &str,
    messages: &[Message],
    actions_taken: usize,
    turn_count: usize,
) {
    let Some(ref ws) = tool_context.workspace else {
        return;
    };

    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let log = serde_json::json!({
        "timestamp": epoch,
        "turn_count": turn_count,
        "actions_taken": actions_taken,
        "system_prompt": system_prompt,
        "messages": messages,
    });

    let content = serde_json::to_string_pretty(&log).unwrap_or_default();
    let file_key = format!("improvement/{epoch}.json");

    let mut ws = ws.write().await;
    ws.set(&file_key, &content);
    if let Err(e) = ws.save() {
        tracing::warn!(
            error = %e,
            file = file_key.as_str(),
            "failed to save reflection log"
        );
    } else {
        tracing::info!(
            file = file_key.as_str(),
            actions_taken,
            "saved reflection log"
        );
    }
}
