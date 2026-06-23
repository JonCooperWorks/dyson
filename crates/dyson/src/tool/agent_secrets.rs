//! Swarm-native agent secret access.
//!
//! This is intentionally a first-party built-in tool, not MCP. Swarm owns
//! storage, encryption, policy, and audit; Dyson only forwards scoped tool
//! calls to Swarm's internal API when the Swarm runtime config is present.

use std::sync::{OnceLock, RwLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::tool::{Tool, ToolContext, ToolOutput};

const ENV_PROXY_URL: &str = "SWARM_PROXY_URL";
const ENV_PROXY_TOKEN: &str = "SWARM_PROXY_TOKEN";
const ENV_INSTANCE_ID: &str = "SWARM_INSTANCE_ID";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSecretsConfig {
    proxy_url: String,
    proxy_token: String,
    instance_id: String,
}

impl AgentSecretsConfig {
    pub fn new(proxy_url: &str, proxy_token: &str, instance_id: &str) -> Option<Self> {
        let proxy_url = proxy_url.trim();
        let proxy_token = proxy_token.trim();
        let instance_id = instance_id.trim();
        if proxy_url.is_empty() || proxy_token.is_empty() || instance_id.is_empty() {
            return None;
        }
        Some(Self {
            proxy_url: proxy_url.to_string(),
            proxy_token: proxy_token.to_string(),
            instance_id: instance_id.to_string(),
        })
    }

    fn from_env() -> Option<Self> {
        Self::new(
            &std::env::var(ENV_PROXY_URL).ok()?,
            &std::env::var(ENV_PROXY_TOKEN).ok()?,
            &std::env::var(ENV_INSTANCE_ID).ok()?,
        )
    }

    fn internal_base(&self) -> Result<String, String> {
        internal_base_from_proxy_url(&self.proxy_url)
    }
}

static RUNTIME_CONFIG: OnceLock<RwLock<Option<AgentSecretsConfig>>> = OnceLock::new();

fn runtime_config() -> &'static RwLock<Option<AgentSecretsConfig>> {
    RUNTIME_CONFIG.get_or_init(|| RwLock::new(None))
}

pub fn set_runtime_config(config: Option<AgentSecretsConfig>) {
    if let Ok(mut guard) = runtime_config().write() {
        *guard = config;
    }
}

pub fn set_runtime_config_from_parts(proxy_url: &str, proxy_token: &str, instance_id: &str) {
    set_runtime_config(AgentSecretsConfig::new(proxy_url, proxy_token, instance_id));
}

pub fn config_snapshot() -> Option<AgentSecretsConfig> {
    runtime_config().read().ok().and_then(|guard| guard.clone())
}

pub struct AgentSecretsTool {
    config: AgentSecretsConfig,
}

impl AgentSecretsTool {
    pub fn from_runtime() -> Option<Self> {
        Self::from_config(config_snapshot().or_else(AgentSecretsConfig::from_env)?)
    }

    pub fn from_config(config: AgentSecretsConfig) -> Option<Self> {
        config.internal_base().ok()?;
        Some(Self { config })
    }

    fn endpoint(&self, name: Option<&str>) -> crate::Result<String> {
        let base = self.config.internal_base().map_err(|e| {
            crate::DysonError::tool("agent_secrets", format!("bad Swarm proxy URL: {e}"))
        })?;
        let root = format!("{}/agent-secrets", base.trim_end_matches('/'));
        Ok(match name {
            Some(name) => format!("{root}/{}", url_component(name)),
            None => root,
        })
    }
}

#[derive(Debug, Deserialize)]
struct AgentSecretInput {
    op: Option<String>,
    name: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SecretValue {
    name: String,
    value: String,
}

#[derive(Debug, Serialize)]
struct SecretSet<'a> {
    value: &'a str,
}

#[async_trait]
impl Tool for AgentSecretsTool {
    fn name(&self) -> &str {
        "agent_secrets"
    }

    fn description(&self) -> &str {
        "List, get, set, or delete this Swarm agent instance's own agent-visible secrets. \
         This is only available in Swarm-managed Dyson sessions."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["list", "get", "set", "delete"],
                    "description": "Operation to perform."
                },
                "name": {
                    "type": "string",
                    "description": "Secret name. Required for get, set, and delete."
                },
                "value": {
                    "type": "string",
                    "description": "Secret value. Required for set."
                }
            },
            "required": ["op"]
        })
    }

    fn agent_only(&self) -> bool {
        true
    }

    async fn run(
        &self,
        input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> crate::Result<ToolOutput> {
        let input = match serde_json::from_value::<AgentSecretInput>(input.clone()) {
            Ok(input) => input,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid input: {e}"))),
        };
        let op = input.op.as_deref().unwrap_or("").trim();
        match op {
            "list" => self.list().await,
            "get" => {
                let name = match required_name(input.name.as_deref(), "get") {
                    Ok(name) => name,
                    Err(out) => return Ok(out),
                };
                self.get(name).await
            }
            "set" => {
                let name = match required_name(input.name.as_deref(), "set") {
                    Ok(name) => name,
                    Err(out) => return Ok(out),
                };
                let Some(value) = input.value.as_deref() else {
                    return Ok(ToolOutput::error("'value' is required for set"));
                };
                self.set(name, value).await
            }
            "delete" => {
                let name = match required_name(input.name.as_deref(), "delete") {
                    Ok(name) => name,
                    Err(out) => return Ok(out),
                };
                self.delete(name).await
            }
            "" => Ok(ToolOutput::error("'op' is required")),
            other => Ok(ToolOutput::error(format!(
                "Unknown op '{other}'. Use list, get, set, or delete."
            ))),
        }
    }
}

impl AgentSecretsTool {
    async fn list(&self) -> crate::Result<ToolOutput> {
        let url = self.endpoint(None)?;
        let text = send_text(
            crate::http::client()
                .get(url)
                .bearer_auth(&self.config.proxy_token),
        )
        .await?;
        Ok(ToolOutput::success(text))
    }

    async fn get(&self, name: &str) -> crate::Result<ToolOutput> {
        let url = self.endpoint(Some(name))?;
        let text = send_text(
            crate::http::client()
                .get(url)
                .bearer_auth(&self.config.proxy_token),
        )
        .await?;
        let value: SecretValue = serde_json::from_str(&text).map_err(|e| {
            crate::DysonError::tool("agent_secrets", format!("parse Swarm response: {e}"))
        })?;
        Ok(ToolOutput::success(format!(
            "{}={}",
            value.name, value.value
        )))
    }

    async fn set(&self, name: &str, value: &str) -> crate::Result<ToolOutput> {
        let url = self.endpoint(Some(name))?;
        let body = SecretSet { value };
        let _ = send_text(
            crate::http::client()
                .put(url)
                .bearer_auth(&self.config.proxy_token)
                .json(&body),
        )
        .await?;
        Ok(ToolOutput::success(format!("Set agent secret '{name}'.")))
    }

    async fn delete(&self, name: &str) -> crate::Result<ToolOutput> {
        let url = self.endpoint(Some(name))?;
        let _ = send_text(
            crate::http::client()
                .delete(url)
                .bearer_auth(&self.config.proxy_token),
        )
        .await?;
        Ok(ToolOutput::success(format!(
            "Deleted agent secret '{name}'."
        )))
    }
}

async fn send_text(request: reqwest::RequestBuilder) -> crate::Result<String> {
    let resp = request
        .send()
        .await
        .map_err(|e| crate::DysonError::tool("agent_secrets", e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| crate::DysonError::tool("agent_secrets", e.to_string()))?;
    if !status.is_success() {
        return Err(crate::DysonError::tool(
            "agent_secrets",
            format!(
                "Swarm agent secret request failed ({status}): {}",
                concise_body(&text)
            ),
        ));
    }
    Ok(text)
}

#[allow(clippy::result_large_err)]
fn required_name<'a>(name: Option<&'a str>, op: &str) -> Result<&'a str, ToolOutput> {
    let name = name.unwrap_or("").trim();
    if name.is_empty() {
        return Err(ToolOutput::error(format!("'name' is required for {op}")));
    }
    if !is_valid_name(name) {
        return Err(ToolOutput::error(
            "name must be 1-128 characters using letters, numbers, '.', '_', or '-'",
        ));
    }
    Ok(name)
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn internal_base_from_proxy_url(proxy_url: &str) -> Result<String, String> {
    let trimmed = proxy_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("empty URL".into());
    }
    if let Some(prefix) = trimmed.strip_suffix("/v1/internal") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    if let Some(prefix) = trimmed.strip_suffix("/v1") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    if let Some((prefix, _)) = trimmed.split_once("/llm") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    Ok(format!("{trimmed}/v1/internal"))
}

fn url_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

fn concise_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    let prefix: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AgentSecretsConfig {
        AgentSecretsConfig::new("https://swarm.example/llm/openrouter", "pt_test", "inst_1")
            .expect("valid config")
    }

    #[test]
    fn config_requires_all_runtime_values() {
        assert!(AgentSecretsConfig::new("", "pt", "i").is_none());
        assert!(AgentSecretsConfig::new("https://s", "", "i").is_none());
        assert!(AgentSecretsConfig::new("https://s", "pt", "").is_none());
        assert!(AgentSecretsTool::from_config(config()).is_some());
    }

    #[test]
    fn agent_secrets_is_not_exposed_to_cli_mcp_bridges() {
        let tool = AgentSecretsTool::from_config(config()).expect("tool");
        assert!(
            tool.agent_only(),
            "agent_secrets is a first-party Swarm tool and must not be forwarded to Claude/Codex MCP"
        );
    }

    #[test]
    fn internal_base_comes_from_proxy_url() {
        assert_eq!(
            internal_base_from_proxy_url("https://swarm.test/llm/openrouter").unwrap(),
            "https://swarm.test/v1/internal"
        );
        assert_eq!(
            internal_base_from_proxy_url("https://swarm.test/v1").unwrap(),
            "https://swarm.test/v1/internal"
        );
    }

    #[tokio::test]
    async fn validation_rejects_missing_required_fields() {
        let tool = AgentSecretsTool::from_config(config()).expect("tool");
        let ctx = ToolContext::for_test(&std::env::temp_dir());
        let out = tool.run(&json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("'op' is required"));

        let out = tool.run(&json!({"op": "get"}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("'name' is required for get"));

        let out = tool
            .run(&json!({"op": "set", "name": "api.token"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("'value' is required for set"));
    }
}
