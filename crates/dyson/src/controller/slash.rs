use std::collections::HashSet;

use serde::Serialize;

use crate::config::Settings;
use crate::skill::local::{LocalSkill, RESERVED_SLASH_COMMANDS};
use crate::tool::ToolOutput;

use super::Output;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SlashCommandInfo {
    pub cmd: String,
    pub desc: String,
    pub src: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableSlashCommand {
    pub command: String,
    pub tool_name: String,
    pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashDispatch {
    NotSlash,
    BuiltinOrUnhandled,
    Handled(String),
}

pub fn builtin_commands() -> Vec<SlashCommandInfo> {
    vec![
        command("/clear", "Clear this conversation", "controller"),
        command(
            "/compact",
            "Summarise transcript in-place to free context",
            "controller",
        ),
        command("/model", "Switch model for this conversation", "controller"),
        command("/models", "List available models", "controller"),
        command("/logs", "Show recent controller logs", "controller"),
        command("/loop", "Schedule a recurring prompt", "controller"),
        command("/stop", "Cancel the current turn", "controller"),
        command("/agents", "List running background agents", "controller"),
        command("/fork-from", "Fork a new conversation from a point", "web"),
    ]
}

pub fn commands_for_settings(settings: &Settings) -> Vec<SlashCommandInfo> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for cmd in builtin_commands() {
        seen.insert(cmd.cmd.clone());
        out.push(cmd);
    }
    for cmd in local_skill_commands(settings) {
        if seen.insert(cmd.cmd.clone()) {
            out.push(cmd);
        }
    }
    out.sort_by(|a, b| a.cmd.cmp(&b.cmd));
    out
}

pub fn find_executable(settings: &Settings, prompt: &str) -> Option<ExecutableSlashCommand> {
    let (command, raw) = parse_command(prompt)?;
    local_skill_commands(settings)
        .into_iter()
        .find(|candidate| candidate.cmd == command)
        .and_then(|candidate| {
            candidate.tool.map(|tool_name| ExecutableSlashCommand {
                command: command.to_string(),
                tool_name,
                raw: raw.to_string(),
            })
        })
}

pub fn known_command(settings: &Settings, command: &str) -> bool {
    RESERVED_SLASH_COMMANDS.contains(&command)
        || local_skill_commands(settings)
            .iter()
            .any(|candidate| candidate.cmd == command)
}

pub fn suggestions(settings: &Settings, command: &str) -> Vec<String> {
    commands_for_settings(settings)
        .into_iter()
        .filter(|candidate| {
            candidate.cmd.starts_with(command)
                || command.starts_with(&candidate.cmd)
                || candidate.cmd.contains(command.trim_start_matches('/'))
        })
        .take(5)
        .map(|candidate| candidate.cmd)
        .collect()
}

pub fn parse_command(prompt: &str) -> Option<(&str, &str)> {
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?;
    let raw = parts.next().unwrap_or("").trim_start();
    Some((command, raw))
}

pub async fn dispatch_executable(
    agent: &mut crate::agent::Agent,
    output: &mut dyn Output,
    settings: &Settings,
    prompt: &str,
    attachments_present: bool,
) -> crate::Result<SlashDispatch> {
    let Some((command, _)) = parse_command(prompt) else {
        return Ok(SlashDispatch::NotSlash);
    };

    if let Some(exec) = find_executable(settings, prompt) {
        let response = if attachments_present {
            format!(
                "{} does not accept file attachments yet. Send the command with text only.",
                exec.command
            )
        } else {
            let tool_output = agent
                .execute_tool_direct(
                    &exec.tool_name,
                    serde_json::json!({
                        "raw": exec.raw,
                        "command": exec.command,
                        "args": exec.raw.split_whitespace().collect::<Vec<_>>(),
                    }),
                )
                .await?;
            emit_side_channels(output, &tool_output)?;
            tool_output.content
        };
        output.text_delta(&response)?;
        agent.append_direct_turn(prompt, &response);
        return Ok(SlashDispatch::Handled(response));
    }

    if known_command(settings, command) {
        return Ok(SlashDispatch::BuiltinOrUnhandled);
    }

    let suggestions = suggestions(settings, command);
    let response = if suggestions.is_empty() {
        format!(
            "Unknown slash command '{command}'. Open the command palette to see available commands."
        )
    } else {
        format!(
            "Unknown slash command '{command}'. Did you mean {}?",
            suggestions.join(", ")
        )
    };
    output.text_delta(&response)?;
    agent.append_direct_turn(prompt, &response);
    Ok(SlashDispatch::Handled(response))
}

fn emit_side_channels(output: &mut dyn Output, tool_output: &ToolOutput) -> crate::Result<()> {
    for file_path in &tool_output.files {
        output.send_file(file_path)?;
    }
    for checkpoint in &tool_output.checkpoints {
        output.checkpoint(checkpoint)?;
    }
    for artefact in &tool_output.artefacts {
        output.send_artefact(artefact)?;
    }
    Ok(())
}

fn local_skill_commands(settings: &Settings) -> Vec<SlashCommandInfo> {
    let Ok(workspace) = crate::workspace::create_workspace(&settings.workspace) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in workspace.skill_dirs() {
        match LocalSkill::from_dir(&dir) {
            Ok(skill) => {
                let Some(cmd) = skill.slash_command() else {
                    continue;
                };
                out.push(SlashCommandInfo {
                    cmd: cmd.to_string(),
                    desc: skill.skill_description().to_string(),
                    src: "skill".to_string(),
                    tool: skill.executable_tool_name(),
                });
            }
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "failed to inspect local skill slash command");
            }
        }
    }
    out
}

fn command(cmd: &str, desc: &str, src: &str) -> SlashCommandInfo {
    SlashCommandInfo {
        cmd: cmd.to_string(),
        desc: desc.to_string(),
        src: src.to_string(),
        tool: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_splits_raw_tail() {
        assert_eq!(
            parse_command("/skill hello world"),
            Some(("/skill", "hello world"))
        );
        assert_eq!(parse_command("  /skill"), Some(("/skill", "")));
        assert_eq!(parse_command("not /skill"), None);
    }
}
