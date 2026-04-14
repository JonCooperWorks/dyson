// ===========================================================================
// TokenBudget — tracks and limits token usage across the agent session.
// ===========================================================================

use crate::error::{DysonError, Result};

/// Token usage tracking and optional budget enforcement.
///
/// Hooks into the agent loop via `process_stream`'s reported `output_tokens`.
/// When a `max_output_tokens` budget is set, the agent loop stops with an
/// error once the cumulative output tokens exceed the limit.
///
/// ## Usage
///
/// ```ignore
/// let mut budget = TokenBudget::default();
/// budget.max_output_tokens = Some(100_000); // cap at 100k output tokens
/// ```
#[derive(Debug, Clone, Default)]
pub struct TokenBudget {
    /// Maximum cumulative output tokens before the agent refuses to continue.
    /// `None` = unlimited (default).
    pub max_output_tokens: Option<usize>,

    /// Cumulative output tokens used across all turns in this session.
    pub output_tokens_used: usize,

    /// Cumulative input tokens used across all turns in this session.
    pub input_tokens_used: usize,

    /// Number of LLM calls made in this session (across all `run()` calls).
    pub llm_calls: usize,
}

impl TokenBudget {
    /// Record tokens from a completed LLM turn.
    ///
    /// Returns `Err` if the budget is exceeded after recording.
    pub fn record(&mut self, output_tokens: usize) -> Result<()> {
        self.output_tokens_used += output_tokens;
        self.llm_calls += 1;
        if let Some(max) = self.max_output_tokens
            && self.output_tokens_used > max
        {
            return Err(DysonError::Llm(format!(
                "token budget exceeded: {}/{max} output tokens used",
                self.output_tokens_used,
            )));
        }
        Ok(())
    }

    /// Check if there's budget remaining (without recording).
    pub const fn has_budget(&self) -> bool {
        match self.max_output_tokens {
            Some(max) => self.output_tokens_used < max,
            None => true,
        }
    }

    /// Record input tokens from a completed LLM turn (informational only).
    pub const fn record_input(&mut self, input_tokens: usize) {
        self.input_tokens_used += input_tokens;
    }

    /// Reset the budget counters (e.g., on `clear()`).
    pub const fn reset(&mut self) {
        self.output_tokens_used = 0;
        self.input_tokens_used = 0;
        self.llm_calls = 0;
    }
}
