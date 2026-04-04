// ===========================================================================
// Dreaming — autonomous background cognition for the agent.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Formalises the concept of "dreaming": background tasks that run
//   concurrently with the main agent loop but never block it.  Dreams
//   are the agent's subconscious — memory consolidation, self-improvement,
//   learning synthesis — all the cognitive housekeeping that happens
//   alongside (or between) waking interactions.
//
//   The key contract: dreams operate *outside* the controller loop.
//   They spawn as fire-and-forget tokio tasks, build their own LLM
//   clients, and communicate only through shared workspace files.
//   Nothing from a dream enters the main conversation history.
//
// Architecture:
//
//   ┌─────────────────────────────────────┐
//   │         Agent (waking loop)         │
//   │  run_inner() → LLM → tools → ...   │
//   │         │                           │
//   │    DreamRunner.fire(event)          │
//   │         │                           │
//   └─────────┼───────────────────────────┘
//             │  tokio::spawn (fire-and-forget)
//             ▼
//   ┌─────────────────────────────────────┐
//   │         Dream (background)          │
//   │  own LLM client, SilentOutput       │
//   │  reads/writes workspace via Arc     │
//   │  never blocks the agent loop        │
//   └─────────────────────────────────────┘
//
// See docs/dreaming.md for the full design document.
// ===========================================================================

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::Result;
use crate::llm::{CompletionConfig, LlmClient};
use crate::tool::ToolContext;

use super::rate_limiter::RateLimitedHandle;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// When a dream should activate.
///
/// Dreams are passive — they don't poll or schedule themselves.  Instead,
/// the [`DreamRunner`] checks each dream's trigger against events that
/// the agent loop emits (turn completed, compaction happened, etc.).
#[derive(Debug, Clone)]
pub enum DreamTrigger {
    /// Fire every N user turns (e.g. memory maintenance every 5 turns).
    EveryNTurns(usize),

    /// Fire after context compaction condenses the conversation.
    AfterCompaction,

    /// Fire when the session is ending (agent teardown / clear).
    OnSessionEnd,
}

/// An event emitted by the agent loop that may activate dreams.
#[derive(Debug, Clone)]
pub enum DreamEvent {
    /// A user turn just completed.
    TurnComplete { turn_count: usize },

    /// Context compaction just ran.
    Compaction,

    /// The session is ending.
    SessionEnd,
}

/// Everything a dream needs to run autonomously.
///
/// Built by the agent and moved into the spawned task.  All fields are
/// owned or `Arc`-wrapped so there's zero borrowing from the agent —
/// the dream is fully independent once spawned.
pub struct DreamContext {
    /// Rate-limited handle to the LLM client, locked to a background priority.
    ///
    /// This is the *only* way to reach the LLM from a dream.  The handle
    /// shares the same rate counter as the main agent loop, so dreams
    /// cannot bypass the provider's rate limits.  Background priority
    /// ensures interactive requests always get priority.
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,

    /// LLM configuration (model, max_tokens, temperature).
    pub config: CompletionConfig,

    /// Tool context with workspace access, working dir, cancellation.
    pub tool_context: ToolContext,

    /// Condensed summary of the conversation (not the full history).
    pub conversation_summary: String,

    /// How many user turns have been processed so far.
    pub turn_count: usize,
}

/// What a dream did — returned for logging and observability.
#[derive(Debug)]
pub struct DreamOutcome {
    /// Which dream produced this outcome.
    pub dream_name: String,

    /// How many tool calls / workspace writes the dream made.
    pub actions_taken: usize,

    /// Wall-clock duration of the dream.
    pub duration: Duration,

    /// Human-readable descriptions of what changed.
    pub artifacts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Dream trait
// ---------------------------------------------------------------------------

/// A unit of autonomous background cognition.
///
/// Implement this trait to define a new kind of dream.  The agent's
/// [`DreamRunner`] will check `trigger()` against incoming events and
/// `tokio::spawn` the dream's `run()` method when it matches.
///
/// # Contract
///
/// - `run()` must be self-contained.  It receives an owned [`DreamContext`]
///   and must build its own LLM client, tools, and output sink.
/// - `run()` must never block the agent loop.  It runs in a spawned task.
/// - Dreams communicate only through the shared workspace (`Arc<RwLock>`).
///   Nothing enters the main conversation history.
#[async_trait]
pub trait Dream: Send + Sync {
    /// Human-readable name for logging and observability.
    fn name(&self) -> &str;

    /// When this dream should activate.
    fn trigger(&self) -> DreamTrigger;

    /// Execute the dream.  Called inside a `tokio::spawn`.
    async fn run(&self, ctx: DreamContext) -> Result<DreamOutcome>;
}

// ---------------------------------------------------------------------------
// DreamRunner — the scheduler that lives on the Agent
// ---------------------------------------------------------------------------

/// Holds registered dreams and fires them when events match their triggers.
///
/// The runner never awaits dream completion — it spawns and moves on.
/// This is the enforcement point for the "never block the controller loop"
/// contract.
#[derive(Default)]
pub struct DreamRunner {
    dreams: Vec<Arc<dyn Dream>>,
}

impl DreamRunner {
    /// Create a runner with no dreams registered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a dream.
    pub fn add(&mut self, dream: Arc<dyn Dream>) {
        tracing::info!(dream = dream.name(), "registered dream");
        self.dreams.push(dream);
    }

    /// Check all registered dreams against an event and spawn any that match.
    ///
    /// This is the only method the agent loop calls.  It returns immediately
    /// after spawning — dreams run in the background with no way to block
    /// the caller.
    pub fn fire(&self, event: &DreamEvent, ctx_factory: impl Fn() -> DreamContext) {
        for dream in &self.dreams {
            if should_activate(dream.trigger(), event) {
                let dream = Arc::clone(dream);
                let ctx = ctx_factory();
                let dream_name = dream.name().to_string();

                tokio::spawn(async move {
                    tracing::info!(dream = dream_name, "dream starting");
                    let start = std::time::Instant::now();

                    match dream.run(ctx).await {
                        Ok(outcome) => {
                            tracing::info!(
                                dream = dream_name,
                                actions = outcome.actions_taken,
                                duration_ms = start.elapsed().as_millis() as u64,
                                artifacts = ?outcome.artifacts,
                                "dream completed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                dream = dream_name,
                                error = %e,
                                duration_ms = start.elapsed().as_millis() as u64,
                                "dream failed"
                            );
                        }
                    }
                });
            }
        }
    }

    /// How many dreams are registered.
    pub fn len(&self) -> usize {
        self.dreams.len()
    }

    /// Whether any dreams are registered.
    pub fn is_empty(&self) -> bool {
        self.dreams.is_empty()
    }
}

/// Check whether a dream's trigger matches an incoming event.
fn should_activate(trigger: DreamTrigger, event: &DreamEvent) -> bool {
    match (trigger, event) {
        (DreamTrigger::EveryNTurns(n), DreamEvent::TurnComplete { turn_count }) => {
            n > 0 && turn_count.is_multiple_of(n)
        }
        (DreamTrigger::AfterCompaction, DreamEvent::Compaction) => true,
        (DreamTrigger::OnSessionEnd, DreamEvent::SessionEnd) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_n_turns_trigger() {
        let trigger = DreamTrigger::EveryNTurns(5);

        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 1 }));
        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 4 }));
        assert!(should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 5 }));
        assert!(should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 10 }));
        assert!(!should_activate(trigger.clone(), &DreamEvent::Compaction));
    }

    #[test]
    fn after_compaction_trigger() {
        let trigger = DreamTrigger::AfterCompaction;

        assert!(should_activate(trigger.clone(), &DreamEvent::Compaction));
        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 5 }));
        assert!(!should_activate(trigger.clone(), &DreamEvent::SessionEnd));
    }

    #[test]
    fn on_session_end_trigger() {
        let trigger = DreamTrigger::OnSessionEnd;

        assert!(should_activate(trigger.clone(), &DreamEvent::SessionEnd));
        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 1 }));
        assert!(!should_activate(trigger.clone(), &DreamEvent::Compaction));
    }

    #[test]
    fn every_n_turns_zero_never_fires() {
        let trigger = DreamTrigger::EveryNTurns(0);

        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 0 }));
        assert!(!should_activate(trigger.clone(), &DreamEvent::TurnComplete { turn_count: 1 }));
    }

    #[test]
    fn dream_runner_empty() {
        let runner = DreamRunner::new();
        assert!(runner.is_empty());
        assert_eq!(runner.len(), 0);
    }
}
