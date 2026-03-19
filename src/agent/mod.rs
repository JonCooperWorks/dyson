// ===========================================================================
// Agent — the streaming tool-use loop at the heart of Dyson.
//
// LEARNING OVERVIEW
//
// What this file does:
//   Implements the core agent loop: send messages to the LLM, stream the
//   response, detect tool calls, execute them through the sandbox, feed
//   results back, repeat.  This is the star in the cage — everything else
//   in Dyson exists to feed this loop.
//
// Module layout:
//   mod.rs             — Agent struct and the loop (this file)
//   stream_handler.rs  — Processes StreamEvents into Messages and ToolCalls
//
// The loop in pseudocode:
//
//   messages = [user_input]
//   for iteration in 0..max_iterations:
//       stream = llm.stream(messages, system_prompt, tools, config)
//       (assistant_msg, tool_calls) = process_stream(stream, output)
//       messages.push(assistant_msg)
//
//       if tool_calls.is_empty():
//           break  // LLM is done, no more tools to run
//
//       for call in tool_calls:
//           decision = sandbox.check(call.name, call.input, ctx)
//           match decision:
//               Allow { input }  → result = tool.run(input, ctx)
//                                   sandbox.after(call.name, &input, &mut result)
//               Deny { reason }  → result = ToolOutput::error(reason)
//               Redirect { .. }  → result = other_tool.run(...)
//                                   sandbox.after(...)
//           messages.push(tool_result(call.id, result))
//
//       // loop — LLM sees tool results on next iteration
//
// Architecture:
//
//   Agent owns:
//     ┌─────────────────────────────────────────────────┐
//     │  client:  Box<dyn LlmClient>                    │
//     │  sandbox: Box<dyn Sandbox>     ← gates all calls│
//     │  skills:  Vec<Box<dyn Skill>>                   │
//     │  tools:   HashMap<name, Arc<dyn Tool>>          │
//     │  tool_definitions: Vec<ToolDefinition>          │
//     │  system_prompt: String                          │
//     │  config: CompletionConfig                       │
//     │  messages: Vec<Message>        ← conversation   │
//     │  max_iterations: usize                          │
//     └─────────────────────────────────────────────────┘
//
// Why does Agent own both skills AND a flat tools map?
//   Skills own tools (for lifecycle management), but the agent needs O(1)
//   lookup by tool name when dispatching calls.  The flat HashMap provides
//   that.  Both hold Arc<dyn Tool> to the same underlying objects — no
//   duplication, just shared references.
// ===========================================================================

pub mod stream_handler;

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::AgentSettings;
use crate::error::{DysonError, Result};
use crate::llm::{CompletionConfig, LlmClient, ToolDefinition};
use crate::message::Message;
use crate::sandbox::{Sandbox, SandboxDecision};
use crate::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolOutput};
use crate::controller::Output;

use self::stream_handler::ToolCall;

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// The streaming tool-use agent — Dyson's core runtime.
///
/// Created once at startup, then `run()` is called for each user message.
/// Conversation history (`messages`) persists across calls for multi-turn
/// conversations.
pub struct Agent {
    /// LLM client for streaming completions.
    client: Box<dyn LlmClient>,

    /// Sandbox that gates all tool execution.
    sandbox: Box<dyn Sandbox>,

    /// Loaded skills (retained for lifecycle: on_unload on shutdown).
    #[allow(dead_code)]
    skills: Vec<Box<dyn Skill>>,

    /// Flat tool lookup map: tool_name → Arc<dyn Tool>.
    ///
    /// Built at construction by flattening all skills' tools.  Shared
    /// ownership (Arc) with the skills — no cloning of tool implementations.
    tools: HashMap<String, Arc<dyn Tool>>,

    /// Tool definitions sent to the LLM so it knows what tools are available.
    tool_definitions: Vec<ToolDefinition>,

    /// Composed system prompt: base prompt + all skill prompt fragments.
    system_prompt: String,

    /// LLM configuration (model, max_tokens, temperature).
    config: CompletionConfig,

    /// Maximum LLM turns per `run()` call.
    max_iterations: usize,

    /// Conversation history.  Persists across `run()` calls.
    messages: Vec<Message>,

    /// Shared tool context (working dir, env, cancellation).
    tool_context: ToolContext,
}

impl Agent {
    /// Construct a new agent from its components.
    ///
    /// This flattens all skills' tools into the agent's lookup map and
    /// composes the system prompt from the base prompt + skill fragments.
    ///
    /// ## Panics
    ///
    /// Does not panic.  Duplicate tool names are handled by last-writer-wins
    /// (later skills override earlier ones), with a warning logged.
    pub fn new(
        client: Box<dyn LlmClient>,
        sandbox: Box<dyn Sandbox>,
        skills: Vec<Box<dyn Skill>>,
        settings: &AgentSettings,
        workspace: Option<std::sync::Arc<tokio::sync::RwLock<Box<dyn crate::workspace::Workspace>>>>,
    ) -> Result<Self> {
        // -- Flatten tools from all skills --
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let mut tool_definitions: Vec<ToolDefinition> = Vec::new();

        for skill in &skills {
            for tool in skill.tools() {
                let name = tool.name().to_string();

                if tools.contains_key(&name) {
                    tracing::warn!(
                        tool = name,
                        skill = skill.name(),
                        "duplicate tool name — overriding previous registration"
                    );
                }

                tool_definitions.push(ToolDefinition {
                    name: name.clone(),
                    description: tool.description().to_string(),
                    input_schema: tool.input_schema(),
                });

                tools.insert(name, Arc::clone(tool));
            }
        }

        tracing::info!(
            tool_count = tools.len(),
            skill_count = skills.len(),
            "agent initialized"
        );

        // -- Compose system prompt --
        //
        // Start with the base prompt, then append each skill's fragment.
        // Skills contribute context like "You have access to bash..." or
        // "The following MCP tools are available...".
        let mut system_prompt = settings.system_prompt.clone();
        for skill in &skills {
            if let Some(fragment) = skill.system_prompt() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(fragment);
            }
        }

        // -- Build completion config --
        let config = CompletionConfig {
            model: settings.model.clone(),
            max_tokens: settings.max_tokens,
            temperature: None, // use provider default
        };

        let mut tool_context = ToolContext::from_cwd()?;
        tool_context.workspace = workspace;

        Ok(Self {
            client,
            sandbox,
            skills,
            tools,
            tool_definitions,
            system_prompt,
            config,
            max_iterations: settings.max_iterations,
            messages: Vec::new(),
            tool_context,
        })
    }

    /// Clear conversation history, starting fresh.
    ///
    /// Resets the agent to a blank slate by dropping all accumulated
    /// messages.  This is the in-memory half of the `/clear` flow — the
    /// caller (e.g. the Telegram controller) is responsible for also
    /// rotating persisted history via [`ChatHistory::rotate`] so old
    /// conversations are archived rather than lost.
    ///
    /// ## Full `/clear` flow (Telegram)
    ///
    /// 1. Remove the [`Agent`] from the in-memory map (drops all state).
    /// 2. Call [`ChatHistory::rotate`] to rename the on-disk history file
    ///    with a timestamp, preserving it for review or RAG indexing.
    /// 3. Reply "Context cleared." to the user.
    /// 4. The next incoming message creates a fresh `Agent` with an empty
    ///    history.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Get the current conversation messages (for persistence).
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replace the conversation history (for restoring from persistence).
    pub fn set_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    /// Run the agent loop for a single user message.
    ///
    /// Appends the user message to the conversation history, then loops:
    /// stream LLM response → execute tool calls → repeat until done.
    ///
    /// The conversation history persists across calls, so the LLM has
    /// context from previous turns.
    ///
    /// ## Returns
    ///
    /// The final assistant text (the last text content from the last
    /// assistant message without tool calls), or an error if something
    /// went wrong.
    pub async fn run(&mut self, user_input: &str, output: &mut dyn Output) -> Result<String> {
        // Append the user's message to history.
        self.messages.push(Message::user(user_input));

        let mut final_text = String::new();

        for iteration in 0..self.max_iterations {
            tracing::info!(
                iteration = iteration,
                model = self.config.model,
                messages = self.messages.len(),
                "starting LLM call"
            );

            // -- Stream LLM response --
            //
            // When the provider handles tools internally (e.g., Claude Code),
            // don't send Dyson's tool definitions — the provider has its own.
            let tools_for_llm = if self.client.handles_tools_internally() {
                &[]
            } else {
                self.tool_definitions.as_slice()
            };

            let stream = self
                .client
                .stream(
                    &self.messages,
                    &self.system_prompt,
                    tools_for_llm,
                    &self.config,
                )
                .await?;

            tracing::info!("streaming response");

            // -- Process the stream into a message + tool calls --
            let (assistant_msg, tool_calls) =
                stream_handler::process_stream(stream, output).await?;

            self.messages.push(assistant_msg.clone());

            // -- If no tool calls, the LLM is done --
            //
            // Also break when the provider handles tools internally (e.g.,
            // Claude Code).  In that case, ToolUse events in the stream are
            // informational — the provider already executed them.  Without
            // this check, Dyson would try to re-execute every tool call and
            // loop until max_iterations, spawning a new subprocess each time.
            if tool_calls.is_empty() || self.client.handles_tools_internally() {
                // Extract the final text from the assistant message.
                for block in &assistant_msg.content {
                    if let crate::message::ContentBlock::Text { text } = block {
                        final_text = text.clone();
                    }
                }
                output.flush()?;
                break;
            }

            // -- Execute tool calls through the sandbox --
            for call in &tool_calls {
                tracing::info!(tool = call.name, id = call.id, "executing tool call");
                let tool_start = std::time::Instant::now();
                let result = self.execute_tool_call(call, output).await;
                let tool_ms = tool_start.elapsed().as_millis();
                match &result {
                    Ok(out) => tracing::info!(
                        tool = call.name,
                        duration_ms = tool_ms,
                        is_error = out.is_error,
                        "tool call finished"
                    ),
                    Err(e) => tracing::error!(
                        tool = call.name,
                        duration_ms = tool_ms,
                        error = %e,
                        "tool call failed"
                    ),
                }

                // Build the tool_result message.
                let tool_result_msg = match result {
                    Ok(ref tool_output) => {
                        Message::tool_result(&call.id, &tool_output.content, tool_output.is_error)
                    }
                    Err(ref e) => Message::tool_result(&call.id, &e.to_string(), true),
                };

                self.messages.push(tool_result_msg);
            }

            // If we're about to hit max iterations, warn.
            if iteration == self.max_iterations - 1 {
                tracing::warn!(
                    max = self.max_iterations,
                    "agent hit maximum iterations — stopping"
                );
                output.error(&DysonError::Llm(format!(
                    "Reached maximum iterations ({}) — stopping",
                    self.max_iterations
                )))?;
            }
        }

        output.flush()?;
        Ok(final_text)
    }

    /// Execute a single tool call, routing through the sandbox.
    ///
    /// ## Flow
    ///
    /// 1. `sandbox.check()` → Allow / Deny / Redirect
    /// 2. On Allow: look up tool → `tool.run()` → `sandbox.after()`
    /// 3. On Deny: return error ToolOutput
    /// 4. On Redirect: look up redirected tool → run it → `sandbox.after()`
    async fn execute_tool_call(
        &self,
        call: &ToolCall,
        output: &mut dyn Output,
    ) -> Result<ToolOutput> {
        // -- Ask the sandbox --
        let decision = self
            .sandbox
            .check(&call.name, &call.input, &self.tool_context)
            .await?;

        match decision {
            SandboxDecision::Allow { input } => {
                // Look up the tool.
                let tool = self.tools.get(&call.name).ok_or_else(|| {
                    DysonError::tool(&call.name, "unknown tool")
                })?;

                // Execute the tool.
                let mut tool_output = match tool.run(input.clone(), &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process through the sandbox.
                self.sandbox
                    .after(&call.name, &input, &mut tool_output)
                    .await?;

                // Display the result.
                output.tool_result(&tool_output)?;

                Ok(tool_output)
            }

            SandboxDecision::Deny { reason } => {
                tracing::info!(tool = call.name, reason = reason, "tool call denied by sandbox");
                let tool_output = ToolOutput::error(format!("Denied by sandbox: {reason}"));
                output.tool_result(&tool_output)?;
                Ok(tool_output)
            }

            SandboxDecision::Redirect { tool_name, input } => {
                tracing::info!(
                    original = call.name,
                    redirected = tool_name,
                    "tool call redirected by sandbox"
                );

                // Look up the redirected tool.
                let tool = self.tools.get(&tool_name).ok_or_else(|| {
                    DysonError::tool(
                        &tool_name,
                        format!("sandbox redirected to unknown tool '{tool_name}'"),
                    )
                })?;

                // Execute the redirected tool.
                let mut tool_output = match tool.run(input.clone(), &self.tool_context).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(e.to_string()),
                };

                // Post-process.
                self.sandbox
                    .after(&tool_name, &input, &mut tool_output)
                    .await?;

                output.tool_result(&tool_output)?;
                Ok(tool_output)
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::stream::{StopReason, StreamEvent};
    use crate::sandbox::no_sandbox::DangerousNoSandbox;
    use crate::skill::builtin::BuiltinSkill;
    use std::pin::Pin;

    // -----------------------------------------------------------------------
    // Mock LLM client that returns a fixed response.
    // -----------------------------------------------------------------------

    struct MockLlm {
        /// Responses to return, in order.  Each call to `stream()` pops
        /// the first entry.
        responses: std::sync::Mutex<Vec<Vec<StreamEvent>>>,
        /// Simulate a provider that handles tools internally (like Claude Code).
        internal_tools: bool,
    }

    impl MockLlm {
        fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                internal_tools: false,
            }
        }

        fn with_internal_tools(responses: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                internal_tools: true,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn stream(
            &self,
            _messages: &[Message],
            _system: &str,
            _tools: &[ToolDefinition],
            _config: &CompletionConfig,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
            let events = self
                .responses
                .lock()
                .unwrap()
                .remove(0);
            Ok(Box::pin(tokio_stream::iter(events.into_iter().map(Ok))))
        }

        fn handles_tools_internally(&self) -> bool {
            self.internal_tools
        }
    }

    // -----------------------------------------------------------------------
    // Mock output
    // -----------------------------------------------------------------------

    struct MockOutput {
        text: String,
    }

    impl MockOutput {
        fn new() -> Self {
            Self { text: String::new() }
        }
    }

    impl Output for MockOutput {
        fn text_delta(&mut self, text: &str) -> Result<()> {
            self.text.push_str(text);
            Ok(())
        }
        fn tool_use_start(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn tool_use_complete(&mut self) -> Result<()> { Ok(()) }
        fn tool_result(&mut self, _: &ToolOutput) -> Result<()> { Ok(()) }
        fn error(&mut self, _: &DysonError) -> Result<()> { Ok(()) }
        fn flush(&mut self) -> Result<()> { Ok(()) }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn simple_text_response() {
        let llm = MockLlm::new(vec![vec![
            StreamEvent::TextDelta("Hello!".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
            },
        ]]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new())];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("hi", &mut output).await.unwrap();
        assert_eq!(result, "Hello!");
        assert_eq!(output.text, "Hello!");
    }

    #[tokio::test]
    async fn tool_call_loop() {
        // First LLM call: request a bash command.
        // Second LLM call: respond with the result.
        let llm = MockLlm::new(vec![
            // Turn 1: LLM calls bash.
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                StreamEvent::ToolUseComplete {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo test_output"}),
                },
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ],
            // Turn 2: LLM responds with text.
            vec![
                StreamEvent::TextDelta("Done.".into()),
                StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        ]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new())];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("run echo test_output", &mut output).await.unwrap();
        assert_eq!(result, "Done.");

        // Conversation should have: user, assistant (tool_use), tool_result, assistant (text)
        assert_eq!(agent.messages.len(), 4);
    }

    #[tokio::test]
    async fn internal_tools_provider_skips_tool_execution() {
        // Simulate a provider like Claude Code that handles tools internally.
        // The stream includes tool events, but the agent loop should NOT try
        // to execute them — it should break after one iteration.
        let llm = MockLlm::with_internal_tools(vec![vec![
            StreamEvent::TextDelta("I'll check. ".into()),
            StreamEvent::ToolUseStart {
                id: "cc_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolUseComplete {
                id: "cc_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            StreamEvent::TextDelta("Here are the files.".into()),
            StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
            },
        ]]);

        let settings = AgentSettings {
            api_key: "test".into(),
            ..Default::default()
        };

        let skills: Vec<Box<dyn Skill>> = vec![Box::new(BuiltinSkill::new())];
        let sandbox: Box<dyn Sandbox> = Box::new(DangerousNoSandbox);
        let mut agent = Agent::new(Box::new(llm), sandbox, skills, &settings, None).unwrap();
        let mut output = MockOutput::new();

        let result = agent.run("list files", &mut output).await.unwrap();

        // Should get the final text, NOT an error from trying to execute "bash".
        assert_eq!(result, "Here are the files.");
        // Only 2 messages: user + assistant (no tool_result messages).
        assert_eq!(agent.messages.len(), 2);
    }
}
