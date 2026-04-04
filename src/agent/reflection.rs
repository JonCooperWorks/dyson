// ===========================================================================
// Reflection — memory maintenance and self-improvement side-channels.
//
// These are side-channel LLM calls that run alongside the main agent loop:
//   - Memory maintenance: review conversation → update MEMORY.md / USER.md
//   - Self-improvement: review conversation → create skills / export data
//   - Background learning: summarise → synthesise into workspace memory
//
// Nothing from these calls enters the main conversation history.
// ===========================================================================

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::{ContentBlock, Message};
use crate::tool::{Tool, ToolContext};

use super::silent_output::SilentOutput;
use super::stream_handler;

impl super::Agent {
    /// Spawn a background task to save learnings from the current conversation.
    ///
    /// Makes its own LLM call to synthesise the conversation into the
    /// workspace's MEMORY.md file.  Builds its own LLM client so it doesn't
    /// share state with the main agent.  Uses [`SilentOutput`] and never
    /// blocks the caller.
    pub(super) fn spawn_save_learnings(&self, reason: &'static str) {
        let workspace = match self.tool_context.workspace {
            Some(ref ws) => Arc::clone(ws),
            None => return,
        };
        if self.messages.is_empty() {
            return;
        }

        let settings = self.agent_settings.clone();
        let config = self.config.clone();
        let summary = Self::summarize_for_reflection(&self.messages);

        tokio::spawn(async move {
            tracing::info!(reason, "background: saving learnings");

            let client = crate::llm::create_client(&settings, None, false);

            if let Err(e) = synthesize_to_workspace(&*client, &config, &summary, &workspace).await {
                tracing::warn!(error = %e, reason, "background learning synthesis failed");
            }

            tracing::info!(reason, "background: learnings saved");
        });
    }

    /// Spawn memory maintenance as a background task.
    ///
    /// Like [`spawn_save_learnings`], this fires off a `tokio::spawn` so the
    /// main agent loop is never blocked.  Uses its own LLM client and
    /// `SilentOutput`.
    pub(super) fn spawn_maintain_memory(&self) {
        if self.tool_context.workspace.is_none() || self.messages.is_empty() {
            return;
        }

        let settings = self.agent_settings.clone();
        let config = self.config.clone();
        let tool_context = ToolContext {
            working_dir: self.tool_context.working_dir.clone(),
            env: self.tool_context.env.clone(),
            cancellation: self.tool_context.cancellation.clone(),
            workspace: self.tool_context.workspace.as_ref().map(Arc::clone),
            depth: self.tool_context.depth,
        };
        let summary = Self::summarize_for_reflection(&self.messages);
        let turn_count = self.turn_count;

        tokio::spawn(async move {
            tracing::info!(turn_count, "background: memory maintenance starting");

            if let Err(e) =
                run_maintain_memory(&settings, &config, &tool_context, &summary).await
            {
                tracing::warn!(error = %e, "background memory maintenance failed");
            }

            tracing::info!(turn_count, "background: memory maintenance complete");
        });
    }

    /// Build the system prompt for the memory maintenance call.
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

    /// Spawn self-improvement reflection as a background task.
    ///
    /// Like [`spawn_maintain_memory`], this fires off a `tokio::spawn` so the
    /// main agent loop is never blocked.
    pub(super) fn spawn_self_improve(&self) {
        if self.tool_context.workspace.is_none() || self.messages.is_empty() {
            return;
        }

        let settings = self.agent_settings.clone();
        let config = self.config.clone();
        let tool_context = ToolContext {
            working_dir: self.tool_context.working_dir.clone(),
            env: self.tool_context.env.clone(),
            cancellation: self.tool_context.cancellation.clone(),
            workspace: self.tool_context.workspace.as_ref().map(Arc::clone),
            depth: self.tool_context.depth,
        };
        let summary = Self::summarize_for_reflection(&self.messages);
        let turn_count = self.turn_count;

        tokio::spawn(async move {
            tracing::info!(turn_count, "background: self-improvement starting");

            if let Err(e) =
                run_self_improve(&settings, &config, &tool_context, &summary, turn_count).await
            {
                tracing::warn!(error = %e, "background self-improvement failed");
            }

            tracing::info!(turn_count, "background: self-improvement complete");
        });
    }

    /// Build the system prompt for the self-improvement reflection call.
    pub(super) async fn build_reflection_system_prompt(ctx: &ToolContext) -> String {
        // List existing skills so the model knows what already exists.
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

    /// Summarize the conversation for the reflection LLM call.
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
                        // Include user messages and short assistant text.
                        let role = match msg.role {
                            crate::message::Role::User => "User",
                            crate::message::Role::Assistant => "Assistant",
                        };
                        // Truncate long text to keep the summary compact.
                        let truncated = if text.len() > 500 {
                            format!("{}...[truncated]", &text[..500])
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
                                &content[..content.len().min(200)]
                            ));
                        } else {
                            let truncated = if content.len() > 200 {
                                format!("{}...", &content[..200])
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
}

// ---------------------------------------------------------------------------
// Background learning synthesis
// ---------------------------------------------------------------------------

/// Summarise a conversation and merge the result into the workspace's
/// MEMORY.md.  This is the workhorse behind `spawn_save_learnings` —
/// it runs entirely in a background task with no tools, just a single
/// LLM call and a workspace write.
pub(super) async fn synthesize_to_workspace(
    client: &dyn LlmClient,
    config: &CompletionConfig,
    conversation_summary: &str,
    workspace: &Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>,
) -> Result<()> {
    // Read the current memory so the LLM can synthesise rather than
    // duplicate.
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

    // Extract the text from the response.
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

    // Write back to workspace.
    {
        let mut ws = workspace.write().await;
        ws.set("MEMORY.md", new_memory.trim());
        ws.save()?;
    }

    tracing::info!(
        new_len = new_memory.len(),
        "MEMORY.md updated by background learning synthesis"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Background memory maintenance
// ---------------------------------------------------------------------------

/// Run memory maintenance entirely in a background task.
///
/// This is the workhorse behind `spawn_maintain_memory`.  It creates its own
/// LLM client, builds the memory tools from scratch (they're stateless unit
/// structs), and runs the same tool-loop that `maintain_memory` uses — but
/// without touching the main agent or its output.
async fn run_maintain_memory(
    settings: &crate::config::AgentSettings,
    config: &CompletionConfig,
    tool_context: &ToolContext,
    summary: &str,
) -> Result<()> {
    // Build system prompt (reads workspace stats).
    let memory_system = super::Agent::build_memory_system_prompt(tool_context).await;

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

    // Fresh LLM client — doesn't share rate-limiting state with the main agent.
    let client = crate::llm::create_client(settings, None, false);

    let mut messages = vec![Message::user(summary)];
    let mut actions_taken = 0usize;

    for _iteration in 0..5u8 {
        let response = match client.stream(&messages, &memory_system, "", &memory_tools, config).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "background memory maintenance LLM call failed");
                return Ok(());
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
                    tracing::warn!(
                        tool = call.name.as_str(),
                        "background memory maintenance: unknown tool"
                    );
                    messages.push(Message::tool_result(
                        &call.id,
                        &format!("unknown tool '{}'", call.name),
                        true,
                    ));
                    continue;
                }
            };

            let result = tool.run(&call.input, tool_context).await;
            let tool_result_msg = match result {
                Ok(ref output) => {
                    actions_taken += 1;
                    tracing::info!(
                        tool = call.name.as_str(),
                        "background memory maintenance: tool ok"
                    );
                    Message::tool_result(&call.id, &output.content, output.is_error)
                }
                Err(ref e) => {
                    tracing::warn!(
                        tool = call.name.as_str(),
                        error = %e,
                        "background memory maintenance: tool failed"
                    );
                    Message::tool_result(&call.id, &e.to_string(), true)
                }
            };
            messages.push(tool_result_msg);
        }
    }

    tracing::info!(
        actions_taken,
        "background memory maintenance complete"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Background self-improvement
// ---------------------------------------------------------------------------

/// Run self-improvement reflection entirely in a background task.
///
/// Creates its own LLM client, builds the reflection tools (skill_create,
/// export_conversation) from scratch, and runs the tool loop — then saves
/// a reflection log to the workspace.
async fn run_self_improve(
    settings: &crate::config::AgentSettings,
    config: &CompletionConfig,
    tool_context: &ToolContext,
    summary: &str,
    turn_count: usize,
) -> Result<()> {
    let reflection_system =
        super::Agent::build_reflection_system_prompt(tool_context).await;

    // Build the two reflection tools as standalone instances.
    let skill_create_tool: Arc<dyn Tool> =
        Arc::new(crate::tool::skill_create::SkillCreateTool);
    let export_tool: Arc<dyn Tool> =
        Arc::new(crate::tool::export_conversation::ExportConversationTool);

    let tool_map: HashMap<String, Arc<dyn Tool>> = HashMap::from([
        (skill_create_tool.name().to_string(), Arc::clone(&skill_create_tool)),
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

    let client = crate::llm::create_client(settings, None, false);

    let mut messages = vec![Message::user(summary)];
    let mut actions_taken = 0usize;

    for iteration in 0..3u8 {
        tracing::info!(iteration, "background self-improvement LLM call");

        let response = match client
            .stream(&messages, &reflection_system, "", &reflection_tools, config)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "background self-improvement LLM call failed");
                return Ok(());
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
                    tracing::warn!(
                        tool = call.name.as_str(),
                        "self-improvement: unknown tool"
                    );
                    messages.push(Message::tool_result(
                        &call.id,
                        &format!("unknown tool '{}'", call.name),
                        true,
                    ));
                    continue;
                }
            };

            let tool_start = std::time::Instant::now();
            let result = tool.run(&call.input, tool_context).await;
            let tool_ms = tool_start.elapsed().as_millis();

            let tool_result_msg = match result {
                Ok(ref output) => {
                    actions_taken += 1;
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

    tracing::info!(actions_taken, "background self-improvement complete");

    // Save reflection log to workspace.
    save_reflection_log(tool_context, &reflection_system, &messages, actions_taken, turn_count)
        .await;

    Ok(())
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
