// ===========================================================================
// Compaction — conversation summarisation and context management.
//
// Implements the five-phase Hermes-style compaction algorithm:
//   1. Prune tool outputs in the middle (cheap, no LLM).
//   2. Identify protected regions (head + tail).
//   3. Summarise the middle section via an LLM call.
//   4. Reassemble: head + [Context Summary] + tail.
//   5. Fix orphaned tool_use/tool_result pairs.
//
// Also includes legacy compaction (summarise entire history) and the
// offline token estimator used to decide when compaction is needed.
// ===========================================================================

use crate::config::CompactionConfig;
use crate::controller::Output;
use crate::error::Result;
use crate::llm::ToolDefinition;
use crate::message::{ContentBlock, Message};

use super::stream_handler;

impl super::Agent {
    /// Compact the conversation using a five-phase Hermes-style algorithm.
    ///
    /// When a `CompactionConfig` is set, the algorithm:
    ///   1. **Prune tool outputs** — replace old `ToolResult` content outside
    ///      protected regions with placeholders (no LLM call).
    ///   2. **Identify regions** — protect the first N messages (head) and the
    ///      most recent messages within a token budget (tail).
    ///   3. **Summarise the middle** — send only the middle section to the LLM
    ///      with a structured prompt (Goal / Progress / Decisions / Files / Next).
    ///   4. **Reassemble** — head + `[Context Summary]` + tail.
    ///   5. **Fix orphaned tool pairs** — insert synthetic `ToolResult` for any
    ///      `ToolUse` whose result was in the summarised section.
    ///
    /// When no `CompactionConfig` is set, falls back to legacy behaviour:
    /// summarise the entire history into a single `[Context Summary]` message.
    ///
    /// ## When to use
    ///
    /// - Automatically: the agent loop triggers compaction when the
    ///   offline-estimated context size exceeds `compaction_config.threshold()`.
    /// - Manually: a controller can call `agent.compact()` directly
    ///   (e.g. in response to a `/compact` command).
    pub async fn compact(&mut self, output: &mut dyn Output) -> Result<()> {
        if self.messages.is_empty() {
            return Ok(());
        }

        // Save learnings in the background before compaction condenses
        // the conversation.  This doesn't block — compaction proceeds
        // immediately while the LLM synthesises in parallel.
        self.spawn_save_learnings("compact");

        tracing::info!(
            messages = self.messages.len(),
            estimated_tokens = self.estimate_context_tokens(&self.system_prompt),
            "compacting conversation context"
        );

        // Dispatch to five-phase or legacy compaction.
        if let Some(config) = self.compaction_config {
            self.compact_hermes(&config, output).await
        } else {
            self.compact_legacy(output).await
        }
    }

    /// Legacy compaction: summarise the entire history into one message.
    async fn compact_legacy(&mut self, output: &mut dyn Output) -> Result<()> {
        // Temporarily move messages out to avoid cloning the entire history.
        // On success we replace them with the summary; on error we restore.
        let messages = std::mem::take(&mut self.messages);
        let old_count = messages.len();

        let summary = match self.summarise_messages(&messages, None, output).await {
            Ok(s) => s,
            Err(e) => {
                self.messages = messages;
                return Err(e);
            }
        };

        if summary.is_empty() {
            tracing::warn!("compaction produced empty summary — keeping original history");
            self.messages = messages;
            return Ok(());
        }

        self.messages
            .push(Message::user(&format!("[Context Summary]\n\n{summary}")));
        self.token_budget.reset();

        tracing::info!(old_messages = old_count, "context compacted (legacy)");
        Ok(())
    }

    /// Five-phase Hermes-style compaction.
    async fn compact_hermes(
        &mut self,
        config: &CompactionConfig,
        output: &mut dyn Output,
    ) -> Result<()> {
        // Phase 2: identify protected regions.
        let head_end = self.head_boundary(config);
        let tail_start = self.tail_boundary(config);

        // If there's no middle section, nothing to summarise.
        if head_end >= tail_start {
            tracing::info!(
                head_end,
                tail_start,
                "protected regions overlap — skipping compaction"
            );
            return Ok(());
        }

        // Phase 1: prune tool outputs in the middle (cheap, no LLM).
        self.prune_tool_outputs(head_end, tail_start);

        // Check for a previous [Context Summary] in the head for iterative merging.
        let previous_summary = self.find_existing_summary(head_end);

        // Phase 3: summarise the middle section.
        // Temporarily take messages to avoid cloning the middle slice.
        let messages = std::mem::take(&mut self.messages);
        let middle = &messages[head_end..tail_start];
        let summary = match self.summarise_messages(middle, previous_summary.as_deref(), output).await {
            Ok(s) => s,
            Err(e) => {
                self.messages = messages;
                return Err(e);
            }
        };

        if summary.is_empty() {
            tracing::warn!("compaction produced empty summary — keeping original history");
            self.messages = messages;
            return Ok(());
        }

        // Phase 4: reassemble — head + summary + tail.
        let old_count = messages.len();
        let mut new_messages = Vec::with_capacity(
            head_end + 1 + (messages.len() - tail_start),
        );

        // Head: keep first N messages, but skip any old [Context Summary].
        for msg in &messages[..head_end] {
            let is_old_summary = msg.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text }
                    if text.starts_with("[Context Summary]"))
            });
            if !is_old_summary {
                new_messages.push(msg.clone());
            }
        }

        // Insert new summary.
        new_messages.push(Message::user(&format!("[Context Summary]\n\n{summary}")));

        // Tail: verbatim.
        new_messages.extend_from_slice(&messages[tail_start..]);

        self.messages = new_messages;

        // Phase 5: fix orphaned tool_use/tool_result pairs.
        self.fix_orphaned_tool_pairs();

        self.token_budget.reset();

        tracing::info!(
            old_messages = old_count,
            new_messages = self.messages.len(),
            "context compacted (hermes)"
        );
        Ok(())
    }

    // -- Compaction helpers --------------------------------------------------

    /// Return the index of the first message NOT in the protected head.
    pub(super) fn head_boundary(&self, config: &CompactionConfig) -> usize {
        config.protect_head.min(self.messages.len())
    }

    /// Return the index of the first message in the protected tail.
    ///
    /// Walks backward from the end, accumulating estimated tokens until
    /// the budget is exhausted.
    pub(super) fn tail_boundary(&self, config: &CompactionConfig) -> usize {
        let mut tokens = 0usize;
        let head_end = self.head_boundary(config);

        for i in (head_end..self.messages.len()).rev() {
            let msg_tokens = self.messages[i].estimate_tokens();
            if tokens + msg_tokens > config.protect_tail_tokens {
                return i + 1;
            }
            tokens += msg_tokens;
        }
        // All non-head messages fit in the tail budget.
        head_end
    }

    /// Phase 1: replace `ToolResult` content in the middle with a placeholder.
    fn prune_tool_outputs(&mut self, head_end: usize, tail_start: usize) {
        for msg in &mut self.messages[head_end..tail_start] {
            for block in &mut msg.content {
                if let ContentBlock::ToolResult { content, .. } = block {
                    *content = "[tool output pruned]".to_string();
                }
            }
        }
    }

    /// Find an existing `[Context Summary]` in the head region.
    fn find_existing_summary(&self, head_end: usize) -> Option<String> {
        for msg in &self.messages[..head_end] {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block
                    && text.starts_with("[Context Summary]")
                {
                    // Strip the prefix to get just the summary body.
                    return Some(
                        text.strip_prefix("[Context Summary]")
                            .unwrap_or(text)
                            .trim()
                            .to_string(),
                    );
                }
            }
        }
        None
    }

    /// Send messages to the LLM for summarisation and return the summary text.
    async fn summarise_messages(
        &self,
        messages: &[Message],
        previous_summary: Option<&str>,
        output: &mut dyn Output,
    ) -> Result<String> {
        let compaction_system = self.build_compaction_prompt(previous_summary);

        let empty_tools: &[ToolDefinition] = &[];
        let response = self
            .client
            .access()?
            .stream(messages, &compaction_system, empty_tools, &self.config)
            .await?;

        let (assistant_msg, _tool_calls, _output_tokens) =
            stream_handler::process_stream(response.stream, output).await?;

        let mut result = String::new();
        for block in &assistant_msg.content {
            if let ContentBlock::Text { text } = block {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(text);
            }
        }
        Ok(result)
    }

    /// Build the system prompt for the summarisation LLM call.
    fn build_compaction_prompt(&self, previous_summary: Option<&str>) -> String {
        let mut prompt = format!(
            "{}\n\n\
             You are being asked to summarise a conversation.  Produce a structured \
             summary with these sections:\n\n\
             ## Goal\nWhat the user is trying to accomplish.\n\n\
             ## Progress\nWhat has been done so far.\n\n\
             ## Key Decisions\nImportant choices and their rationale.\n\n\
             ## Files Modified\nList of files touched and changes made.\n\n\
             ## Next Steps\nWhat was about to happen or still needs to happen.\n\n\
             Be concise but thorough.  Do NOT call any tools.  \
             Do NOT ask questions.  Just summarise.",
            self.system_prompt,
        );

        if let Some(prev) = previous_summary {
            prompt.push_str(&format!(
                "\n\n---\n\n\
                 ## Previous context summary\n\n\
                 The following is a summary from a previous compaction.  Merge it \
                 with the new conversation into a single updated summary:\n\n{prev}"
            ));
        }

        prompt
    }

    /// Phase 5: fix orphaned tool_use/tool_result pairs after reassembly.
    ///
    /// After compaction the middle section is gone, so:
    /// - A `ToolUse` in the head whose `ToolResult` was in the middle now
    ///   has no matching result.  We insert a synthetic one.
    /// - A `ToolResult` in the tail whose `ToolUse` was in the middle now
    ///   has no matching call.  We remove it.
    pub(super) fn fix_orphaned_tool_pairs(&mut self) {
        use std::collections::{HashMap, HashSet};

        // Collect all tool_use IDs (with positions) and tool_result IDs.
        let mut tool_use_positions: HashMap<String, usize> = HashMap::new();
        let mut tool_result_ids: HashSet<String> = HashSet::new();

        for (pos, msg) in self.messages.iter().enumerate() {
            for block in &msg.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        tool_use_positions.insert(id.clone(), pos);
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        tool_result_ids.insert(tool_use_id.clone());
                    }
                    _ => {}
                }
            }
        }

        // Find orphaned tool_use IDs (no matching result) with their positions.
        let mut orphaned_uses: Vec<(String, usize)> = tool_use_positions
            .iter()
            .filter(|(id, _)| !tool_result_ids.contains(id.as_str()))
            .map(|(id, &pos)| (id.clone(), pos))
            .collect();

        // Find orphaned tool_result IDs (no matching use).
        let tool_use_ids: HashSet<&str> =
            tool_use_positions.keys().map(|s| s.as_str()).collect();
        let orphaned_results: HashSet<String> = tool_result_ids
            .iter()
            .filter(|id| !tool_use_ids.contains(id.as_str()))
            .cloned()
            .collect();

        // Insert synthetic results for orphaned uses.
        // Sort by descending position so earlier inserts don't shift later indices.
        orphaned_uses.sort_by(|a, b| b.1.cmp(&a.1));
        for (orphan_id, pos) in &orphaned_uses {
            let synthetic = Message::tool_result(
                orphan_id,
                "[result included in context summary]",
                false,
            );
            self.messages.insert(pos + 1, synthetic);
        }

        // Remove orphaned results (results whose tool_use was in the middle).
        self.messages.retain(|msg| {
            !msg.content.iter().all(|b| {
                matches!(b, ContentBlock::ToolResult { tool_use_id, .. }
                    if orphaned_results.contains(tool_use_id))
            })
        });
    }

    /// Estimate the total token count of the current context that would be
    /// sent to the LLM (messages + system prompt + tool definitions).
    ///
    /// This is a local/offline estimate using whitespace splitting — no API
    /// call needed.  Used to decide whether to compact before the next call.
    pub(super) fn estimate_context_tokens(&self, system_prompt: &str) -> usize {
        let system_tokens = system_prompt.split_whitespace().count();

        let message_tokens: usize = self.messages.iter().map(|m| m.estimate_tokens()).sum();

        system_tokens + message_tokens + self.cached_tool_tokens
    }
}
