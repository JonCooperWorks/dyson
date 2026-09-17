//! Shared command parsing and execution for interaction channels.
use super::{
    ClientRegistry, Output, background, list_providers, read_log_tail, spawn_background_agent,
};
use crate::config::Settings;
use std::path::Path;

/// Parse a `/model` command argument into (provider_name, optional_model).
///
/// Resolution order:
/// 1. If the first word matches a provider name, use it as provider.
///    A second word (if present) is the model within that provider.
/// 2. Otherwise, check if the entire argument is a model in the current provider.
/// 3. If neither matches, return an error.
fn parse_model_command(
    args: &str,
    providers: &std::collections::HashMap<String, crate::config::ProviderConfig>,
    current_provider: &str,
) -> Result<(String, Option<String>), String> {
    let args = args.trim();
    if args.is_empty() {
        return Err("Usage: /model <provider> [model]  or  /model <model>".to_string());
    }

    // Split into at most 2 parts: potential provider + potential model.
    let mut parts = args.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap(); // always present after empty check
    let second = parts.next().map(str::trim);

    // Case 1: first word is a known provider name.
    if providers.contains_key(first) {
        return Ok((
            first.to_string(),
            second.map(std::string::ToString::to_string),
        ));
    }

    // Case 2: not a provider — try as a model in the current provider.
    if let Some(pc) = providers.get(current_provider)
        && pc.models.iter().any(|m| m == args)
    {
        return Ok((current_provider.to_string(), Some(args.to_string())));
    }

    Err(format!("unknown provider or model '{first}'"))
}

/// A provider and its models, ready for rendering by controllers.
pub struct ProviderInfo {
    pub name: String,
    pub provider_type: String,
    pub models: Vec<ModelInfo>,
}

/// A single model within a provider.
pub struct ModelInfo {
    pub name: String,
    pub active: bool,
}

/// Result of executing a shared command.
///
/// Controllers match on this to render output and add controller-specific
/// side effects (e.g. Telegram persists chat history after compaction).
pub enum CommandResult {
    /// `/clear` succeeded — agent context was cleared.
    Cleared,
    /// `/compact` succeeded — conversation was compacted.
    Compacted,
    /// `/compact` failed.
    CompactError(String),
    /// `/models` — list of providers and their models.
    ModelList { providers: Vec<ProviderInfo> },
    /// `/model` succeeded — switched to a new provider/model.
    ModelSwitched {
        provider_name: String,
        provider_type: String,
        model: String,
    },
    /// `/model` failed — could not switch.
    ModelSwitchError(String),
    /// `/model` with bad arguments.
    ModelParseError(String),
    /// `/model` with no arguments — show usage.
    ModelUsage,
    /// `/logs` — recent log lines.
    Logs(String),
    /// `/logs` failed — could not read log file.
    LogsError(String),
    /// `/loop` succeeded — background agent spawned.
    LoopStarted {
        id: u64,
        prompt_preview: String,
        chat_id: String,
    },
    /// `/loop` failed.
    LoopError(String),
    /// `/agents` — list of running background agents.
    AgentList {
        agents: Vec<background::BackgroundAgentListEntry>,
    },
    /// `/stop` succeeded — agent cancellation requested.
    AgentStopped { id: u64 },
    /// `/stop` failed — invalid ID or agent not found.
    StopError(String),
    /// Input was not a shared command — controller should handle it.
    NotHandled,
}

/// Execute a lock-free command that doesn't need the agent.
///
/// Handles: `/logs`, `/agents`, `/loop`, `/stop`, `/models`.
/// Returns `NotHandled` for commands that require the agent lock.
/// Callback invoked when a background agent finishes.
///
/// Receives the agent ID and either the final response text (`Ok`) or a
/// human-readable failure message (`Err`).  Controllers use this to surface
/// background agent results to the user who spawned them.
pub type BackgroundCompletion = std::sync::Arc<dyn Fn(u64, Result<String, String>) + Send + Sync>;

/// Parse only the exact command spellings accepted by the controllers.
/// A literal space separates arguments; `/models extra` is not `/models`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BuiltinCommand<'a> {
    Logs(usize),
    Models,
    Agents,
    Loop(&'a str),
    Stop(Option<&'a str>),
    Clear,
    Compact,
    Model(&'a str),
    Unhandled,
}

impl<'a> BuiltinCommand<'a> {
    pub(super) fn parse(input: &'a str) -> Self {
        let (name, args) = match input.split_once(' ') {
            Some((name, args)) => (name, Some(args.trim())),
            None => (input, None),
        };
        match (name, args) {
            ("/logs", args) => Self::Logs(args.and_then(|s| s.parse().ok()).unwrap_or(20)),
            ("/models", None) => Self::Models,
            ("/agents", None) => Self::Agents,
            ("/loop", args) => Self::Loop(args.unwrap_or("")),
            ("/stop", args) => Self::Stop(args),
            ("/clear", None) => Self::Clear,
            ("/compact", None) => Self::Compact,
            ("/model", args) => Self::Model(args.unwrap_or("")),
            _ => Self::Unhandled,
        }
    }
}

pub async fn execute_lockfree_command(
    input: &str,
    settings: &Settings,
    registry: &ClientRegistry,
    bg_registry: &std::sync::Arc<background::BackgroundAgentRegistry>,
    on_complete: Option<BackgroundCompletion>,
) -> CommandResult {
    match BuiltinCommand::parse(input) {
        BuiltinCommand::Logs(n) => {
            match tokio::task::spawn_blocking(move || read_log_tail(n)).await {
                Ok(Ok(lines)) => CommandResult::Logs(lines),
                Ok(Err(e)) => CommandResult::LogsError(e),
                Err(e) => CommandResult::LogsError(format!("task failed: {e}")),
            }
        }
        // Active highlighting needs an agent; the lock-free list is informational.
        BuiltinCommand::Models => CommandResult::ModelList {
            providers: list_providers(settings)
                .into_iter()
                .map(|(name, pc)| ProviderInfo {
                    name: name.to_owned(),
                    provider_type: format!("{:?}", pc.provider_type),
                    models: pc
                        .models
                        .iter()
                        .map(|name| ModelInfo {
                            name: name.clone(),
                            active: false,
                        })
                        .collect(),
                })
                .collect(),
        },
        BuiltinCommand::Agents => CommandResult::AgentList {
            agents: bg_registry.list(),
        },
        BuiltinCommand::Loop("") => CommandResult::LoopError("usage: /loop <prompt>".into()),
        BuiltinCommand::Loop(prompt) => {
            spawn_background_agent(prompt, settings, registry, bg_registry, on_complete).await
        }
        BuiltinCommand::Stop(None) => CommandResult::StopError("usage: /stop <id>".into()),
        BuiltinCommand::Stop(Some(args)) => match args.parse() {
            Ok(id) => match bg_registry.stop(id) {
                Ok(()) => CommandResult::AgentStopped { id },
                Err(e) => CommandResult::StopError(e),
            },
            Err(_) => CommandResult::StopError("invalid agent ID".into()),
        },
        _ => CommandResult::NotHandled,
    }
}

/// Mutable model-switching state threaded through agent commands.
pub struct ModelState<'a> {
    pub provider: &'a mut String,
    pub model: &'a mut String,
    pub config_path: Option<&'a Path>,
}

/// Execute a command that requires the agent lock.
///
/// Handles: `/clear`, `/compact`, `/model`.
/// Returns `NotHandled` for everything else.
pub async fn execute_agent_command(
    input: &str,
    agent: &mut crate::agent::Agent,
    output: &mut dyn Output,
    settings: &Settings,
    ms: &mut ModelState<'_>,
    registry: &ClientRegistry,
) -> CommandResult {
    let args = match BuiltinCommand::parse(input) {
        BuiltinCommand::Clear => {
            agent.clear();
            return CommandResult::Cleared;
        }
        BuiltinCommand::Compact => {
            return match agent.compact(output).await {
                Ok(()) => CommandResult::Compacted,
                Err(e) => CommandResult::CompactError(e.to_string()),
            };
        }
        BuiltinCommand::Model("") => return CommandResult::ModelUsage,
        BuiltinCommand::Model(args) => args,
        _ => return CommandResult::NotHandled,
    };
    let (target_provider, target_model) =
        match parse_model_command(args, &settings.providers, ms.provider) {
            Ok(parsed) => parsed,
            Err(e) => return CommandResult::ModelParseError(e),
        };
    let pc = match settings.providers.get(&target_provider) {
        Some(pc) => pc,
        None => {
            return CommandResult::ModelSwitchError(format!(
                "unknown provider '{target_provider}'"
            ));
        }
    };
    let resolved = target_model
        .as_deref()
        .unwrap_or_else(|| pc.default_model())
        .to_string();
    match registry.get(&target_provider) {
        Ok(handle) => {
            agent.swap_client(handle, &resolved, &pc.provider_type);
            *ms.model = resolved.clone();
            *ms.provider = target_provider.clone();
            if let Some(cp) = ms.config_path {
                crate::config::loader::persist_model_selection(cp, &target_provider, &resolved);
            }
            CommandResult::ModelSwitched {
                provider_name: target_provider,
                provider_type: format!("{:?}", pc.provider_type),
                model: resolved,
            }
        }
        Err(e) => CommandResult::ModelSwitchError(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::BuiltinCommand as Command;

    #[test]
    fn command_boundaries_and_arguments_preserve_wire_grammar() {
        let cases = [
            ("/logs", Command::Logs(20)),
            ("/logs  3 ", Command::Logs(3)),
            ("/logs invalid", Command::Logs(20)),
            ("/logs 0", Command::Logs(0)),
            ("/models", Command::Models),
            ("/agents", Command::Agents),
            ("/models extra", Command::Unhandled),
            ("/agents ", Command::Unhandled),
            ("/loop", Command::Loop("")),
            ("/loop   ", Command::Loop("")),
            ("/loop  hello world ", Command::Loop("hello world")),
            ("/stop", Command::Stop(None)),
            ("/stop ", Command::Stop(Some(""))),
            ("/stop  42", Command::Stop(Some("42"))),
            ("/model", Command::Model("")),
            ("/model provider model", Command::Model("provider model")),
            ("/clear", Command::Clear),
            ("/compact", Command::Compact),
            ("/clear ", Command::Unhandled),
            ("/compact extra", Command::Unhandled),
            ("/logs\t3", Command::Unhandled),
            ("/modelsuffix", Command::Unhandled),
            ("plain text", Command::Unhandled),
        ];
        for (input, expected) in cases {
            assert_eq!(Command::parse(input), expected, "{input:?}");
        }
    }
}
