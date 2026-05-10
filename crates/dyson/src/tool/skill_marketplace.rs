//! Swarm-backed skill marketplace installer.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::tool::{Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize)]
struct SourceList {
    #[serde(default)]
    sources: Vec<MarketplaceSource>,
}

#[derive(Debug, Deserialize)]
struct MarketplaceSource {
    id: String,
    source_type: String,
    location: String,
    #[serde(default)]
    is_default: bool,
}

#[derive(Debug, Deserialize)]
struct CatalogListing {
    #[serde(default)]
    skills: Vec<CatalogSkill>,
    #[serde(default)]
    errors: Vec<CatalogError>,
}

#[derive(Debug, Deserialize)]
struct CatalogError {
    marketplace_id: String,
    error: String,
}

#[derive(Debug, Deserialize)]
struct CatalogSkill {
    marketplace_id: String,
    marketplace_name: String,
    name: String,
    version: String,
    description: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    sha256: Option<String>,
    content_type: String,
}

#[derive(Debug, Deserialize)]
struct SkillDetail {
    skill: CatalogSkill,
    preview: String,
    computed_sha256: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillBody {
    pub marketplace_id: String,
    pub marketplace_name: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub declared_sha256: Option<String>,
    pub computed_sha256: String,
    pub skill_md: String,
}

#[derive(Debug, Serialize)]
pub struct SkillInstallOutcome {
    pub installed: bool,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct SkillRemoveOutcome {
    pub uninstalled: bool,
    pub skill: String,
}

#[derive(Debug)]
pub enum SkillInstallError {
    AlreadyInstalled { current_version: Option<String> },
    Invalid(String),
    Workspace(crate::DysonError),
}

#[derive(Debug)]
pub enum SkillRemoveError {
    Invalid(String),
    NotInstalled,
    Workspace(crate::DysonError),
}

impl From<crate::DysonError> for SkillInstallError {
    fn from(value: crate::DysonError) -> Self {
        Self::Workspace(value)
    }
}

impl From<crate::DysonError> for SkillRemoveError {
    fn from(value: crate::DysonError) -> Self {
        Self::Workspace(value)
    }
}

pub struct SkillMarketplaceTool;

#[async_trait]
impl Tool for SkillMarketplaceTool {
    fn name(&self) -> &str {
        "skill_marketplace"
    }

    fn description(&self) -> &str {
        "List, inspect, and install SKILL.md skills from the Swarm-hosted skill marketplace. \
         Installed skills are written into the local workspace under skills/<name>/ and can be \
         loaded with load_skill after reload."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["list_sources", "list", "show", "install", "update", "remove"],
                    "description": "Marketplace operation."
                },
                "marketplace": {
                    "type": "string",
                    "description": "Marketplace id, required for show/install/update."
                },
                "skill": {
                    "type": "string",
                    "description": "Skill name, required for show/install/update/remove."
                },
                "force": {
                    "type": "boolean",
                    "description": "Overwrite an existing local skill on install."
                }
            },
            "required": ["op"]
        })
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let op = input["op"].as_str().unwrap_or("").trim();
        let marketplace = input["marketplace"].as_str().unwrap_or("").trim();
        let skill = input["skill"].as_str().unwrap_or("").trim();
        let force = input["force"].as_bool().unwrap_or(false);
        match op {
            "list_sources" => list_sources().await,
            "list" => list_skills().await,
            "show" => show_skill(marketplace, skill).await,
            "install" => install_skill(ctx, marketplace, skill, force).await,
            "update" => install_skill(ctx, marketplace, skill, true).await,
            "remove" => remove_skill(ctx, skill).await,
            "" => Ok(ToolOutput::error("'op' is required")),
            other => Ok(ToolOutput::error(format!(
                "Unknown op '{other}'. Use list_sources, list, show, install, update, or remove."
            ))),
        }
    }
}

async fn list_sources() -> crate::Result<ToolOutput> {
    let sources: SourceList = swarm_get("skill-marketplaces").await?;
    if sources.sources.is_empty() {
        return Ok(ToolOutput::success(
            "No Swarm skill marketplaces configured.",
        ));
    }
    let mut out = String::from("Swarm skill marketplaces:\n");
    for source in sources.sources {
        let default = if source.is_default { " default" } else { "" };
        out.push_str(&format!(
            "- {} ({}){}: {}\n",
            source.id, source.source_type, default, source.location
        ));
    }
    Ok(ToolOutput::success(out))
}

async fn list_skills() -> crate::Result<ToolOutput> {
    let listing: CatalogListing = swarm_get("skill-marketplaces/skills").await?;
    let mut out = String::new();
    if listing.skills.is_empty() {
        out.push_str("No marketplace skills available.\n");
    } else {
        out.push_str("Available marketplace skills:\n");
        for skill in listing.skills {
            let tags = if skill.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", skill.tags.join(", "))
            };
            out.push_str(&format!(
                "- {}/{} v{}{}: {}\n",
                skill.marketplace_id, skill.name, skill.version, tags, skill.description
            ));
        }
    }
    if !listing.errors.is_empty() {
        out.push_str("\nMarketplace errors:\n");
        for err in listing.errors {
            out.push_str(&format!("- {}: {}\n", err.marketplace_id, err.error));
        }
    }
    Ok(ToolOutput::success(out))
}

async fn show_skill(marketplace: &str, skill: &str) -> crate::Result<ToolOutput> {
    if marketplace.is_empty() || skill.is_empty() {
        return Ok(ToolOutput::error(
            "'marketplace' and 'skill' are required for show",
        ));
    }
    let path = format!(
        "skill-marketplaces/{}/skills/{}",
        url_component(marketplace),
        url_component(skill)
    );
    let detail: SkillDetail = swarm_get(&path).await?;
    let mut out = format!(
        "{} / {} v{}\n{}\ncontent: {}, sha256: {}\n\n",
        detail.skill.marketplace_name,
        detail.skill.name,
        detail.skill.version,
        detail.skill.description,
        detail.skill.content_type,
        detail
            .skill
            .sha256
            .as_deref()
            .unwrap_or(&detail.computed_sha256)
    );
    out.push_str("Preview:\n");
    out.push_str(&detail.preview);
    Ok(ToolOutput::success(out))
}

async fn install_skill(
    ctx: &ToolContext,
    marketplace: &str,
    skill: &str,
    force: bool,
) -> crate::Result<ToolOutput> {
    if marketplace.is_empty() || skill.is_empty() {
        return Ok(ToolOutput::error(
            "'marketplace' and 'skill' are required for install",
        ));
    }
    let body_path = format!(
        "skill-marketplaces/{}/skills/{}/content",
        url_component(marketplace),
        url_component(skill)
    );
    let package: SkillBody = swarm_get(&body_path).await?;
    let ws = ctx.workspace("skill_marketplace")?;
    match install_skill_package_to_workspace(ws, marketplace, skill, package, force).await {
        Ok(_outcome) => Ok(ToolOutput::success(format!(
            "Installed skill '{}' from marketplace '{}'. It is available at skills/{}/SKILL.md and will appear in <available_skills> after reload.",
            skill, marketplace, skill
        ))),
        Err(SkillInstallError::AlreadyInstalled { .. }) => Ok(ToolOutput::error(format!(
            "Skill '{}' already exists. Use op='update' or force=true to overwrite.",
            skill
        ))),
        Err(SkillInstallError::Invalid(msg)) => Ok(ToolOutput::error(msg)),
        Err(SkillInstallError::Workspace(err)) => Err(err),
    }
}

pub async fn install_skill_package_to_workspace(
    workspace: &crate::workspace::WorkspaceHandle,
    marketplace: &str,
    skill: &str,
    package: SkillBody,
    force: bool,
) -> Result<SkillInstallOutcome, SkillInstallError> {
    if package.marketplace_id != marketplace {
        return Err(SkillInstallError::Invalid(format!(
            "Marketplace response id mismatch: requested '{}', returned '{}'",
            marketplace, package.marketplace_id
        )));
    }
    if package.name != skill {
        return Err(SkillInstallError::Invalid(format!(
            "Marketplace skill name mismatch: requested '{}', returned '{}'",
            skill, package.name
        )));
    }
    if !is_valid_skill_name(&package.name) {
        return Err(SkillInstallError::Invalid(format!(
            "Marketplace returned invalid skill name '{}'",
            package.name
        )));
    }
    let computed = sha256_hex(package.skill_md.as_bytes());
    if computed != package.computed_sha256 {
        return Err(SkillInstallError::Invalid(format!(
            "Swarm package hash mismatch: local {computed}, swarm {}",
            package.computed_sha256
        )));
    }
    if let Some(declared) = package.declared_sha256.as_deref()
        && !declared.eq_ignore_ascii_case(&computed)
    {
        return Err(SkillInstallError::Invalid(format!(
            "Declared package hash mismatch: declared {declared}, computed {computed}"
        )));
    }

    let mut ws = workspace.write().await;
    let skill_key = format!("skills/{}/SKILL.md", package.name);
    let metadata_key = format!("skills/{}/dyson-skill.json", package.name);
    if ws.get(&skill_key).is_some() && !force {
        return Err(SkillInstallError::AlreadyInstalled {
            current_version: installed_skill_version(ws.get(&metadata_key).as_deref()),
        });
    }
    ws.set(&skill_key, &package.skill_md);
    let version = package.version.clone();
    let metadata = json!({
        "schema_version": 1,
        "name": package.name,
        "version": version,
        "description": package.description,
        "origin": {
            "kind": "marketplace",
            "marketplace_id": package.marketplace_id,
            "marketplace_name": package.marketplace_name,
            "sha256": computed,
        },
        "installed_at": now_rfc3339ish(),
    });
    let metadata = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| "{}".into());
    ws.set(&metadata_key, &format!("{metadata}\n"));
    ws.save()?;
    ws.journal(&format!(
        "Installed marketplace skill '{}' from '{}'.",
        skill, marketplace
    ));
    ws.save()?;
    Ok(SkillInstallOutcome {
        installed: true,
        version,
        sha256: computed,
    })
}

fn installed_skill_version(metadata: Option<&str>) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(metadata?).ok()?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

async fn remove_skill(ctx: &ToolContext, skill: &str) -> crate::Result<ToolOutput> {
    if skill.is_empty() {
        return Ok(ToolOutput::error("'skill' is required for remove"));
    }
    let ws = ctx.workspace("skill_marketplace")?;
    match remove_skill_from_workspace(ws, skill).await {
        Ok(_) => Ok(ToolOutput::success(format!(
            "Removed skill '{skill}' from skills/{skill}/SKILL.md."
        ))),
        Err(SkillRemoveError::Invalid(msg)) => Ok(ToolOutput::error(msg)),
        Err(SkillRemoveError::NotInstalled) => Ok(ToolOutput::error(format!(
            "Skill '{skill}' is not installed."
        ))),
        Err(SkillRemoveError::Workspace(err)) => Err(err),
    }
}

pub async fn remove_skill_from_workspace(
    workspace: &crate::workspace::WorkspaceHandle,
    skill: &str,
) -> Result<SkillRemoveOutcome, SkillRemoveError> {
    if !is_valid_skill_name(skill) {
        return Err(SkillRemoveError::Invalid(format!(
            "Invalid skill name '{skill}'"
        )));
    }
    let mut ws = workspace.write().await;
    let skill_key = format!("skills/{skill}/SKILL.md");
    let metadata_key = format!("skills/{skill}/dyson-skill.json");
    let removed_body = ws.remove(&skill_key)?;
    let removed_metadata = ws.remove(&metadata_key)?;
    if !removed_body && !removed_metadata {
        return Err(SkillRemoveError::NotInstalled);
    }
    ws.journal(&format!("Removed skill '{skill}'."));
    ws.save()?;
    Ok(SkillRemoveOutcome {
        uninstalled: true,
        skill: skill.to_owned(),
    })
}

async fn swarm_get<T: for<'de> Deserialize<'de>>(path: &str) -> crate::Result<T> {
    let cfg = crate::swarm_state_sync::config_snapshot()
        .or_else(crate::swarm_state_sync::config_from_env)
        .ok_or_else(|| {
            crate::DysonError::tool(
                "skill_marketplace",
                "Swarm state sync is not configured; marketplace catalog is unavailable",
            )
        })?;
    let base = marketplace_base_from_state_url(&cfg.url).map_err(|e| {
        crate::DysonError::tool(
            "skill_marketplace",
            format!("bad Swarm state sync URL: {e}"),
        )
    })?;
    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let resp = crate::http::client()
        .get(&url)
        .bearer_auth(&cfg.token)
        .send()
        .await
        .map_err(|e| crate::DysonError::tool("skill_marketplace", e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| crate::DysonError::tool("skill_marketplace", e.to_string()))?;
    if !status.is_success() {
        return Err(crate::DysonError::tool(
            "skill_marketplace",
            format!("Swarm marketplace request failed ({status}): {text}"),
        ));
    }
    serde_json::from_str(&text)
        .map_err(|e| crate::DysonError::tool("skill_marketplace", format!("parse response: {e}")))
}

fn marketplace_base_from_state_url(url: &str) -> Result<String, String> {
    if let Some(prefix) = url.strip_suffix("/v1/internal/state/file") {
        return Ok(format!("{prefix}/v1/internal"));
    }
    if let Some(prefix) = url.strip_suffix("/state/file") {
        return Ok(prefix.to_string());
    }
    Err("expected URL ending in /v1/internal/state/file".into())
}

fn url_component(value: &str) -> String {
    value.replace('/', "%2F")
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

fn now_rfc3339ish() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_marketplace_base_from_state_url() {
        assert_eq!(
            marketplace_base_from_state_url("https://swarm.test/v1/internal/state/file").unwrap(),
            "https://swarm.test/v1/internal"
        );
    }

    #[test]
    fn validates_skill_names() {
        assert!(is_valid_skill_name("code-review"));
        assert!(!is_valid_skill_name("../bad"));
        assert!(!is_valid_skill_name("-bad"));
        assert!(!is_valid_skill_name("bad-"));
    }

    #[tokio::test]
    async fn installs_package_and_metadata() {
        let skill_md = "---\nname: code-review\ndescription: Review code.\n---\n\nRead the diff.";
        let computed_sha256 = sha256_hex(skill_md.as_bytes());
        let package = SkillBody {
            marketplace_id: "official".into(),
            marketplace_name: "Official Skills".into(),
            name: "code-review".into(),
            version: "1.0.0".into(),
            description: "Review code.".into(),
            declared_sha256: Some(computed_sha256.clone()),
            computed_sha256: computed_sha256.clone(),
            skill_md: skill_md.into(),
        };
        let ws = crate::workspace::InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);

        let workspace = ctx.workspace("test").unwrap().clone();
        let out = install_skill_package_to_workspace(
            &workspace,
            "official",
            "code-review",
            package,
            false,
        )
        .await
        .unwrap();
        assert!(out.installed);
        assert_eq!(out.version, "1.0.0");
        assert_eq!(out.sha256, computed_sha256);

        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        assert_eq!(ws.get("skills/code-review/SKILL.md").unwrap(), skill_md);
        let metadata: serde_json::Value =
            serde_json::from_str(&ws.get("skills/code-review/dyson-skill.json").unwrap()).unwrap();
        assert_eq!(metadata["name"], "code-review");
        assert_eq!(metadata["version"], "1.0.0");
        assert_eq!(metadata["description"], "Review code.");
        assert_eq!(metadata["origin"]["kind"], "marketplace");
        assert_eq!(metadata["origin"]["marketplace_id"], "official");
        assert_eq!(metadata["origin"]["marketplace_name"], "Official Skills");
        assert_eq!(metadata["origin"]["sha256"], computed_sha256);
    }

    #[tokio::test]
    async fn rejects_mismatched_marketplace_response() {
        let skill_md = "body";
        let package = SkillBody {
            marketplace_id: "other".into(),
            marketplace_name: "Other".into(),
            name: "code-review".into(),
            version: "1.0.0".into(),
            description: "Review code.".into(),
            declared_sha256: None,
            computed_sha256: sha256_hex(skill_md.as_bytes()),
            skill_md: skill_md.into(),
        };
        let ws = crate::workspace::InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);

        let workspace = ctx.workspace("test").unwrap().clone();
        let out = install_skill_package_to_workspace(
            &workspace,
            "official",
            "code-review",
            package,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(out, SkillInstallError::Invalid(msg) if msg.contains("Marketplace response id mismatch"))
        );
    }

    #[tokio::test]
    async fn removes_installed_skill_files() {
        let ws = crate::workspace::InMemoryWorkspace::new()
            .with_file("skills/code-review/SKILL.md", "body")
            .with_file("skills/code-review/dyson-skill.json", "{}");
        let ctx = ToolContext::for_test_with_workspace(ws);

        let out = remove_skill(&ctx, "code-review").await.unwrap();
        assert!(!out.is_error, "{}", out.content);

        let ws = ctx.workspace.unwrap();
        let ws = ws.read().await;
        assert!(ws.get("skills/code-review/SKILL.md").is_none());
        assert!(ws.get("skills/code-review/dyson-skill.json").is_none());
    }

    #[tokio::test]
    async fn remove_workspace_helper_reports_missing_skill() {
        let ws = crate::workspace::InMemoryWorkspace::new();
        let ctx = ToolContext::for_test_with_workspace(ws);
        let workspace = ctx.workspace("test").unwrap().clone();

        let out = remove_skill_from_workspace(&workspace, "code-review")
            .await
            .unwrap_err();
        assert!(matches!(out, SkillRemoveError::NotInstalled));
    }
}
