use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{DysonError, Result};

pub const DEFAULT_SCRIPT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_SCRIPT_TIMEOUT_MS: u64 = 300_000;

pub const RESERVED_SLASH_COMMANDS: &[&str] = &[
    "/agents",
    "/clear",
    "/compact",
    "/fork-from",
    "/logs",
    "/loop",
    "/model",
    "/models",
    "/stop",
];

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSkillManifest {
    pub schema_version: u64,
    pub name: String,
    pub description: String,
    pub slash_command: Option<String>,
    pub execution: SkillExecution,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkillExecution {
    None,
    Script(ScriptExecution),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptExecution {
    pub entrypoint: String,
    pub argument_mode: ArgumentMode,
    pub input_schema: Option<serde_json::Value>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentMode {
    Raw,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    #[serde(default)]
    schema_version: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    slash_command: Option<String>,
    #[serde(default)]
    execution: Option<RawExecution>,
}

#[derive(Debug, Deserialize)]
struct RawExecution {
    kind: String,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    argument_mode: Option<ArgumentMode>,
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl LocalSkillManifest {
    pub fn instruction_only(name: &str, description: &str) -> Result<Self> {
        validate_skill_name(name)?;
        Ok(Self {
            schema_version: 1,
            name: name.to_string(),
            description: description.to_string(),
            slash_command: None,
            execution: SkillExecution::None,
        })
    }

    pub fn from_json(
        content: &str,
        dir_name: &str,
        fallback_description: &str,
        skill_dir: &Path,
    ) -> Result<Self> {
        let raw: RawManifest = serde_json::from_str(content)
            .map_err(|e| DysonError::Config(format!("invalid dyson-skill.json: {e}")))?;

        let schema_version = raw.schema_version.unwrap_or(1);
        if schema_version > 2 {
            return Err(DysonError::Config(format!(
                "unsupported skill manifest schema_version {schema_version}"
            )));
        }

        let name = raw.name.unwrap_or_else(|| dir_name.to_string());
        if name != dir_name {
            return Err(DysonError::Config(format!(
                "skill manifest name '{name}' must match directory name '{dir_name}'"
            )));
        }
        validate_skill_name(&name)?;

        let description = raw
            .description
            .unwrap_or_else(|| fallback_description.to_string())
            .trim()
            .to_string();

        let slash_command = raw
            .slash_command
            .map(|cmd| validate_slash_command(&cmd).map(|_| cmd))
            .transpose()?;

        let execution = match raw.execution {
            None => SkillExecution::None,
            Some(raw) if raw.kind == "none" => SkillExecution::None,
            Some(raw) if raw.kind == "script" => {
                let entrypoint = raw.entrypoint.unwrap_or_default();
                validate_entrypoint(&entrypoint)?;
                let resolved = skill_dir.join(&entrypoint);
                if !resolved.is_file() {
                    return Err(DysonError::Config(format!(
                        "skill '{name}' script entrypoint '{}' does not exist",
                        resolved.display()
                    )));
                }
                SkillExecution::Script(ScriptExecution {
                    entrypoint,
                    argument_mode: raw.argument_mode.unwrap_or(ArgumentMode::Raw),
                    input_schema: raw.input_schema,
                    timeout_ms: normalize_timeout_ms(raw.timeout_ms),
                })
            }
            Some(raw) => {
                return Err(DysonError::Config(format!(
                    "unsupported skill execution kind '{}'",
                    raw.kind
                )));
            }
        };

        if matches!(execution, SkillExecution::Script(_)) && slash_command.is_none() {
            return Err(DysonError::Config(format!(
                "skill '{name}' has script execution but no slash_command"
            )));
        }

        Ok(Self {
            schema_version,
            name,
            description,
            slash_command,
            execution,
        })
    }

    pub fn tool_name(&self) -> String {
        tool_name_for_skill(&self.name)
    }
}

pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.ends_with('-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(DysonError::Config(
            "invalid skill name: use lowercase letters, digits, and hyphens only".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_slash_command(command: &str) -> Result<()> {
    let Some(rest) = command.strip_prefix('/') else {
        return Err(DysonError::Config(
            "slash_command must start with '/'".to_string(),
        ));
    };
    if rest.is_empty()
        || rest.starts_with('-')
        || rest.ends_with('-')
        || rest.len() > 63
        || !rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(DysonError::Config(format!(
            "invalid slash_command '{command}': use /lowercase-name"
        )));
    }
    if RESERVED_SLASH_COMMANDS.contains(&command) {
        return Err(DysonError::Config(format!(
            "slash_command '{command}' collides with a built-in command"
        )));
    }
    Ok(())
}

pub fn validate_entrypoint(entrypoint: &str) -> Result<()> {
    if entrypoint.is_empty() || entrypoint.contains('\0') || entrypoint.contains('\\') {
        return Err(DysonError::Config(
            "execution.entrypoint must be a non-empty relative path".to_string(),
        ));
    }
    let path = Path::new(entrypoint);
    if path.is_absolute() {
        return Err(DysonError::Config(
            "execution.entrypoint must not be absolute".to_string(),
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(DysonError::Config(format!(
                    "execution.entrypoint '{entrypoint}' must not contain traversal"
                )));
            }
        }
    }
    Ok(())
}

pub fn resolved_entrypoint(skill_dir: &Path, entrypoint: &str) -> Result<PathBuf> {
    validate_entrypoint(entrypoint)?;
    Ok(skill_dir.join(entrypoint))
}

pub fn normalize_timeout_ms(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS)
        .clamp(1, MAX_SCRIPT_TIMEOUT_MS)
}

pub fn tool_name_for_skill(name: &str) -> String {
    format!("skill_{}", name.replace('-', "_"))
}
