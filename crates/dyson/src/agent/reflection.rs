// ===========================================================================
// Reflection — Dream implementations for memory and self-improvement.
//
// This module contains the three built-in dreams:
//
//   1. LearningSynthesisDream  — single LLM call that curates MEMORY.md
//      (Keep / Refine / Discard judgment) and folds in new signal from the
//      conversation.  Fires after compaction.
//
//   2. MemoryMaintenanceDream  — mini agent loop with workspace tools to
//      curate MEMORY.md, USER.md, and overflow notes.  Applies the same
//      Keep / Refine / Discard judgment and writes an audit trail to
//      improvement/{epoch}.json.  Fires every N user turns.
//
//   3. SelfImprovementDream    — mini agent loop that creates skills
//      based on patterns and user feedback.  Fires every 2N turns.
//
// Curation (not merging) is the key verb for dreams 1 and 2: they walk
// the existing file and actively delete low-value entries rather than
// only appending new ones.  The shared rules live in CURATION_RULES.
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
// Curation rules — shared by every dream that mutates MEMORY.md.
//
// Both LearningSynthesisDream and MemoryMaintenanceDream embed this exact
// text in their system prompts so the signal/noise judgment stays in sync.
// The rules treat curation as a Keep/Refine/Discard decision, not a merge,
// and they explicitly forbid time-of-day or entry-age heuristics so that
// work done at night is never penalised.
// ---------------------------------------------------------------------------

const CURATION_RULES: &str = "\
## Curation rules (Keep / Refine / Discard)\n\
\n\
Your job is not to merge — it is to **curate**.  Walk the existing file \
line by line and apply this judgment to every block:\n\
\n\
### KEEP (signal)\n\
- Reusable procedures and non-obvious debugging patterns\n\
- Architecture decisions and their rationale\n\
- Bug fixes with an identified root cause\n\
- User preferences, workflow patterns, communication style\n\
- Technical constraints future sessions must respect\n\
- References to active skills or tools\n\
- Context and approaches from interactions the user rated highly (+2 or +3)\n\
\n\
### REFINE (compress, preserve meaning)\n\
- Verbose entries restating the same fact — collapse to one bullet\n\
- Overlapping entries covering the same decision — merge into one\n\
- Multi-line narratives that can become a single bullet\n\
\n\
### DISCARD (noise)\n\
- Chitchat, greetings, acknowledgments\n\
- Brainstorming that did not lead to a decision\n\
- Failed experiments with no transferable lesson\n\
- Duplicates of entries already present elsewhere in the file\n\
- Raw tool output (summarise the insight, not the data)\n\
- One-off session state: temp paths, ephemeral task IDs, scratch values\n\
- Approaches from poorly-rated interactions (-2 or -3) where the method was \
clearly wrong (but preserve the lesson if one was learned)\n\
\n\
### CRITICAL — the anti-timestamp rule\n\
NEVER use time-of-day, day-of-week, date, or apparent entry age as a \
pruning signal.  We work heavily at night.  An entry written at 3 AM is \
as valuable as one written at 3 PM.  Judge purely on content value.  If \
you cannot justify a DISCARD decision without referencing *when* an \
entry was made, you must KEEP it.\n\
\n\
### Rating-informed priority\n\
When feedback ratings appear in the conversation summary, treat them as \
strong signal.  Highly-rated interactions (+2 to +3) indicate approaches, \
patterns, and preferences confirmed valuable by the user — prioritise \
preserving that context.  Poorly-rated interactions (-2 to -3) indicate \
dissatisfaction — look for what went wrong and preserve the lesson, but \
discard the failed approach itself.  Unrated interactions should be \
judged purely on content value as usual.\n\
\n\
### Fuzzy sizing\n\
Aim for the soft target but do not truncate valuable signal to hit it.  \
Overflow up to the ceiling is allowed when every extra character is \
paying its way.  2,700 chars of valuable context beats 2,470 chars of \
truncated context.  Only the hard ceiling is a refusal — the tool will \
warn you when you are over soft target but still within the ceiling, \
and that is a fine place to land.\n\
\n\
### Safety rails\n\
- Preserve all user preferences and explicit user instructions.\n\
- When in doubt, KEEP.  A false discard is worse than a slightly full file.\n\
- Doing nothing is acceptable if the file is already well curated.\n";

// ---------------------------------------------------------------------------
// Shared mini agent loop — used by MemoryMaintenance and SelfImprovement
// ---------------------------------------------------------------------------

/// Run a mini agent loop: LLM calls tools in a loop until it stops or hits
/// `max_iterations`.  Returns `(actions_taken, per-tool artifacts)`.
async fn run_mini_loop(
    ctx: &DreamContext,
    system_prompt: &str,
    tools: Vec<Arc<dyn Tool>>,
    initial_message: &str,
    max_iterations: u8,
    dream_label: &str,
) -> Result<(usize, Vec<String>)> {
    let tool_defs: Vec<ToolDefinition> = tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
            agent_only: false,
        })
        .collect();

    let tool_map: HashMap<String, Arc<dyn Tool>> = tools
        .into_iter()
        .map(|t| (t.name().to_string(), t))
        .collect();

    let mut messages = vec![Message::user(initial_message)];
    let mut actions_taken = 0usize;
    let mut artifacts = Vec::new();

    for _iteration in 0..max_iterations {
        let client = match ctx.client.access() {
            Ok(guard) => guard,
            Err(e) => {
                tracing::info!(error = %e, "{dream_label}: rate limited, stopping early");
                break;
            }
        };

        let response = match client
            .stream(&messages, system_prompt, "", &tool_defs, &ctx.config)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "{dream_label} LLM call failed");
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
                    tracing::warn!(tool = call.name.as_str(), "{dream_label}: unknown tool");
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
                    tracing::info!(tool = call.name.as_str(), "{dream_label}: tool ok");
                    Message::tool_result(&call.id, &output.content, output.is_error)
                }
                Err(ref e) => {
                    tracing::warn!(tool = call.name.as_str(), error = %e, "{dream_label}: tool failed");
                    Message::tool_result(&call.id, &e.to_string(), true)
                }
            };
            artifacts.push(call.name.clone());
            messages.push(tool_result_msg);
        }
    }

    Ok((actions_taken, artifacts))
}

// ---------------------------------------------------------------------------
// Conversation summariser — shared by all dreams
// ---------------------------------------------------------------------------

use crate::util::truncate_to_char_boundary as truncate_str;

/// Summarize a conversation for dream consumption.
///
/// Instead of sending the full message history (which could be huge),
/// build a condensed representation: user goals, tools used, outcomes.
pub(super) fn summarize_for_reflection(messages: &[Message]) -> String {
    let mut summary = String::from(
        "Review this conversation and decide whether to create/improve a skill \
         or export training data.  Here is what happened:\n\n",
    );

    let mut tool_call_count = 0usize;
    let mut tool_error_count = 0usize;
    let mut tools_used: Vec<String> = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let role = match msg.role {
                        crate::message::Role::User => "User",
                        crate::message::Role::Assistant => "Assistant",
                    };
                    let truncated = truncate_str(text, 500);
                    summary.push_str(&format!("{role}: {truncated}\n\n"));
                }
                ContentBlock::ToolUse { name, .. } => {
                    tool_call_count += 1;
                    if !tools_used.contains(name) {
                        tools_used.push(name.clone());
                    }
                    summary.push_str(&format!("[Tool call: {name}]\n"));
                }
                ContentBlock::ToolResult {
                    is_error, content, ..
                } => {
                    if *is_error {
                        tool_error_count += 1;
                    }
                    summary.push_str(&format!(
                        "[Tool {}: {}]\n",
                        if *is_error { "error" } else { "result" },
                        truncate_str(content, 200)
                    ));
                }
                ContentBlock::Image { media_type, .. } => {
                    summary.push_str(&format!("[Image: {media_type}]\n"));
                }
                ContentBlock::Document { extracted_text, .. } => {
                    summary.push_str(&format!(
                        "[PDF: {} chars extracted]\n",
                        extracted_text.len()
                    ));
                }
                ContentBlock::Thinking { .. } => {}
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

/// Format feedback entries into a textual summary for dream prompts.
pub(super) fn format_feedback_summary(
    entries: &[crate::feedback::FeedbackEntry],
    message_count: usize,
) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let total_rated = entries.len();
    let avg_score: f64 =
        entries.iter().map(|e| e.score as f64).sum::<f64>() / total_rated as f64;

    // Score distribution: indices 0..7 map to scores -3..+3.
    let mut dist = [0u32; 7];
    for e in entries {
        let idx = (e.score + 3) as usize;
        if idx < 7 {
            dist[idx] += 1;
        }
    }

    use std::fmt::Write;
    let mut out = format!(
        "\n\n## User feedback ratings\n\
         Average score: {avg_score:+.1} ({total_rated} rated turns out of {message_count} messages)\n"
    );

    let mut fmt_turns = |label: &str, pred: fn(i8) -> bool| {
        let mut first = true;
        for e in entries {
            if pred(e.score) {
                if first {
                    let _ = write!(out, "{label}: turns {}", e.turn_index);
                    first = false;
                } else {
                    let _ = write!(out, ", {}", e.turn_index);
                }
            }
        }
        if !first {
            out.push('\n');
        }
    };

    fmt_turns("Highly rated (score >= +2)", |s| s >= 2);
    fmt_turns("Poorly rated (score <= -1)", |s| s <= -1);

    let _ = writeln!(
        out,
        "Score distribution: [-3:{}, -2:{}, -1:{}, 0:{}, +1:{}, +2:{}, +3:{}]",
        dist[0], dist[1], dist[2], dist[3], dist[4], dist[5], dist[6]
    );

    out
}

// ===========================================================================
// 1. LearningSynthesisDream
// ===========================================================================

/// Curates MEMORY.md via a single LLM call.
///
/// This is the lightest dream — no tools, just one prompt asking the LLM
/// to apply a Keep / Refine / Discard judgment to the existing memory file
/// and fold in genuine new signal from the conversation.  Fires after
/// compaction so curation happens before the conversation is condensed.
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

/// Curates workspace memory files using tools and an agent mini-loop.
///
/// Unlike LearningSynthesisDream (which does a single write), this dream
/// runs a mini agent loop with workspace_view, workspace_update,
/// workspace_search, and memory_search tools.  It reads the existing
/// files, applies a Keep / Refine / Discard judgment (see
/// [`CURATION_RULES`]), rewrites them via `workspace_update mode=set`,
/// and moves overflow to `memory/notes/` when even the ceiling is tight.
///
/// When the curation pass takes at least one action, the dream writes an
/// audit trail to `improvement/{epoch}.json` via [`save_reflection_log`]
/// so humans can inspect what was kept and what was discarded.
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
        let memory_system = build_memory_system_prompt(&ctx.tool_context).await;

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tool::workspace_view::WorkspaceViewTool),
            Arc::new(crate::tool::workspace_update::WorkspaceUpdateTool),
            Arc::new(crate::tool::workspace_search::WorkspaceSearchTool),
            Arc::new(crate::tool::memory_search::MemorySearchTool),
        ];

        let (actions_taken, _) = run_mini_loop(
            &ctx,
            &memory_system,
            tools,
            &ctx.conversation_summary,
            5,
            "memory maintenance",
        )
        .await?;

        // Audit trail: record the curation pass so the user can see what
        // Keep/Refine/Discard judgment was applied.  Only log when the
        // dream actually took action — silent no-ops don't need a log.
        if actions_taken > 0 {
            save_reflection_log(
                &ctx.tool_context,
                &memory_system,
                &[Message::user(&ctx.conversation_summary)],
                actions_taken,
                ctx.turn_count,
            )
            .await;
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
    let (current_memory, soft_target, ceiling) = {
        let ws = workspace.read().await;
        let current = ws.get("MEMORY.md").unwrap_or_default();
        let target = ws.char_limit("MEMORY.md").unwrap_or(0);
        let ceil = ws.char_ceiling("MEMORY.md").unwrap_or(target);
        (current, target, ceil)
    };
    let current_len = current_memory.chars().count();

    let system = format!(
        "You are a memory curator.  You will receive the current contents \
         of MEMORY.md and a summary of a conversation that is about to be \
         cleared.  Your job is to produce a **curated** MEMORY.md that \
         applies a Keep / Refine / Discard judgment to the existing file \
         AND folds in any genuine new signal from the conversation.\n\n\
         ## File sizing\n\
         - Soft target: {soft_target} chars (aim here)\n\
         - Hard ceiling: {ceiling} chars (never exceed)\n\
         - Current size: {current_len} chars\n\n\
         ## Output format\n\
         - Output ONLY the new file contents.  No commentary, no markdown fences.\n\
         - Synthesise — do not append.  Rewrite the file as one coherent document.\n\
         - If nothing needs changing, output the existing file unchanged.\n\n\
         {CURATION_RULES}"
    );

    let user_message = format!(
        "## Current MEMORY.md ({current_len} chars, soft target {soft_target}, ceiling {ceiling})\n\n\
         {current_memory}\n\n\
         ## Conversation summary\n\n{conversation_summary}"
    );

    let messages = vec![Message::user(&user_message)];
    let empty_tools: Vec<ToolDefinition> = Vec::new();

    let response = client
        .stream(&messages, &system, "", &empty_tools, config)
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

    let trimmed = new_memory.trim();
    if trimmed.is_empty() {
        tracing::info!("learning synthesis produced empty output — skipping write");
        return Ok(());
    }

    // Skip the write if content hasn't changed.
    if current_memory.trim() == trimmed {
        tracing::info!("learning synthesis produced no changes — skipping write");
        return Ok(());
    }

    {
        let mut ws = workspace.write().await;
        ws.set("MEMORY.md", trimmed);
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
    let (memory_usage, memory_target, memory_ceiling, user_usage, user_target, user_ceiling) =
        if let Some(ref ws) = ctx.workspace {
            let ws = ws.read().await;
            let mu = ws.get("MEMORY.md").map(|c| c.chars().count()).unwrap_or(0);
            let mt = ws.char_limit("MEMORY.md").unwrap_or(0);
            let mc = ws.char_ceiling("MEMORY.md").unwrap_or(mt);
            let uu = ws.get("USER.md").map(|c| c.chars().count()).unwrap_or(0);
            let ut = ws.char_limit("USER.md").unwrap_or(0);
            let uc = ws.char_ceiling("USER.md").unwrap_or(ut);
            (mu, mt, mc, uu, ut, uc)
        } else {
            (0, 0, 0, 0, 0, 0)
        };

    format!(
        "You are a memory curator for an AI agent.  Your job is not merely to \
         record new information — it is to **curate** the existing memory files \
         using a strict signal-vs-noise judgment, and to fold in genuine new \
         signal from the conversation that just happened.\n\n\
         You have workspace tools to view and rewrite the agent's memory files.\n\n\
         ## Files (soft target / hard ceiling, current usage)\n\
         - **MEMORY.md** — current {memory_usage} chars, soft target {memory_target}, \
           hard ceiling {memory_ceiling}.  Curated long-term memory: key facts, \
           decisions, patterns, lessons.\n\
         - **USER.md** — current {user_usage} chars, soft target {user_target}, \
           hard ceiling {user_ceiling}.  What the agent knows about the user.\n\
         - **memory/notes/*.md** — unlimited overflow storage, searchable via \
           memory_search.  Park detail here when even the ceiling is tight.\n\n\
         ## Process\n\
         1. Call workspace_view to read the current memory files.\n\
         2. For every existing line or block, silently label it KEEP, REFINE, \
            or DISCARD using the rules below.\n\
         3. Call workspace_update with mode \"set\" to rewrite the file: \
            DISCARD lines removed, REFINE lines compressed, KEEP lines \
            preserved, new signal folded in.\n\
         4. If a file is already well curated, doing nothing is the right \
            answer.\n\n\
         {CURATION_RULES}"
    )
}

// ===========================================================================
// 3. SelfImprovementDream
// ===========================================================================

/// Reviews conversation and creates skills based on patterns and user feedback.
///
/// Runs a mini agent loop with the skill_create tool.
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

        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::tool::skill_create::SkillCreateTool),
        ];

        let (actions_taken, artifacts) = run_mini_loop(
            &ctx,
            &reflection_system,
            tools,
            &ctx.conversation_summary,
            3,
            "self-improvement",
        )
        .await?;

        save_reflection_log(
            &ctx.tool_context,
            &reflection_system,
            &[Message::user(&ctx.conversation_summary)],
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
         a conversation that just happened and decide whether to create or improve \
         a skill.\n\n\
         You have one tool:\n\
         - **skill_create**: Create or improve a SKILL.md file in the workspace.  \
           Skills are system prompt fragments that auto-load on startup, teaching \
           the agent reusable procedures.\n\n\
         ## When to create a skill\n\
         - The agent solved a complex, multi-step task that it might encounter again\n\
         - The agent discovered a non-obvious procedure or debugging pattern\n\
         - The agent found a domain-specific workflow worth encoding\n\
         - The conversation contains highly-rated interactions (+2 or +3) that \
           demonstrate a pattern worth codifying\n\n\
         ## When to improve a skill\n\
         - An existing skill's instructions were insufficient and the agent had to \
           improvise — capture what worked\n\
         - The agent found a better approach than what the skill describes\n\
         - Poorly-rated interactions suggest an existing skill gives bad advice\n\n\
         ## Rating-informed decisions\n\
         When feedback ratings are present in the conversation summary, use them \
         as strong signal: highly-rated turns (+2, +3) suggest patterns worth \
         encoding; poorly-rated turns (-2, -3) suggest a skill needs correction \
         or a new skill should capture a better approach.\n\n\
         ## When to do nothing\n\
         - The conversation was trivial (simple Q&A, one-step tasks)\n\
         - A matching skill already exists and doesn't need improvement\n\
         - The conversation was mostly errors or failed attempts with no \
           transferable lesson\n\
         - Ratings are neutral or absent and no notable pattern emerged\n\n\
         Doing nothing is the right choice most of the time.  Only act when there's \
         genuine value in persisting knowledge.\n\n\
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
