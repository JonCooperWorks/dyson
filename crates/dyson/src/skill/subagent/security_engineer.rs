// ===========================================================================
// Security engineer staged research harness.
//
// The parent-facing tool remains `security_engineer`, but the implementation is
// no longer a single broad "review this repo" child agent. The orchestrator
// drives a staged harness:
//
//   Recon -> Hunt -> Validate -> Gapfill -> Dedupe -> Trace -> Feedback -> Report
//
// Each stage writes a durable JSON checkpoint under the Dyson workspace's kb/
// tree. In Swarm mode that path is mirrored by the existing state-file sync
// worker, so checkpoints survive instance recreate/rollout without adding a
// security-specific Swarm API.
// ===========================================================================

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::orchestrator::{OrchestratorConfig, OrchestratorHarness, OrchestratorInput};
use super::{ChildSpawn, spawn_child};
use crate::agent::rate_limiter::RateLimitedHandle;
use crate::config::{AgentSettings, LlmProvider};
use crate::error::Result;
use crate::llm::LlmClient;
use crate::message::{Artefact, ArtefactKind};
use crate::sandbox::Sandbox;
use crate::tool::{CheckpointEvent, Tool, ToolOutput};
use crate::workspace::WorkspaceHandle;

const DIRECT_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "search_files",
    "list_files",
    "ast_describe",
    "ast_query",
    "attack_surface_analyzer",
    "exploit_builder",
    "taint_trace",
    "dependency_scan",
];

const CHECKPOINT_PREFIX: &str = "kb/security-harness/checkpoints";
pub const SECURITY_HARNESS_SCHEMA_VERSION: u32 = 1;
pub const SECURITY_HARNESS_VERSION: &str = "security-harness-v1";
pub const DEFAULT_HUNT_BATCH_SIZE: usize = 4;
pub const DEFAULT_MAX_HUNT_BATCHES: usize = 4;

const STAGES: &[SecurityHarnessStage] = &[
    SecurityHarnessStage::Recon,
    SecurityHarnessStage::Hunt,
    SecurityHarnessStage::Validate,
    SecurityHarnessStage::Gapfill,
    SecurityHarnessStage::Dedupe,
    SecurityHarnessStage::Trace,
    SecurityHarnessStage::Feedback,
    SecurityHarnessStage::Report,
];

const VULNERABILITY_TAXONOMY: &[VulnerabilityClassDefinition] = &[
    VulnerabilityClassDefinition {
        id: "auth_authorization",
        name: "Authentication and authorization",
        description: "Missing auth checks, confused deputy paths, IDOR/BOLA, tenant boundary bypass, role confusion, token audience/provider confusion, revoked-token acceptance, and bearer leakage.",
        examples: &[
            "missing auth checks",
            "confused deputy",
            "IDOR/BOLA",
            "tenant boundary bypass",
            "admin/user role confusion",
            "token audience/provider confusion",
            "stale token acceptance",
            "bearer leakage",
        ],
        evidence_requirements: &[
            "entry route or caller",
            "authorization decision",
            "owner/role/tenant predicate",
            "attacker-controlled identifier or token path",
        ],
        detector_keywords: &[
            "auth",
            "bearer",
            "owner",
            "tenant",
            "admin",
            "role",
            "token",
            "permission",
            "acl",
        ],
    },
    VulnerabilityClassDefinition {
        id: "session_oauth_csrf",
        name: "Session, OAuth, and CSRF",
        description: "Open redirects, OAuth state/nonce/PKCE mistakes, callback auth assumptions, CSRF gaps, cookie scope/SameSite/Secure pitfalls, and login/session fixation.",
        examples: &[
            "open redirect",
            "unsafe return_to",
            "OAuth state",
            "nonce",
            "PKCE",
            "CSRF",
            "cookie SameSite",
            "session fixation",
        ],
        evidence_requirements: &[
            "callback or state-changing route",
            "state/nonce/cookie handling",
            "redirect or CSRF validation",
            "attacker-controlled request parameter",
        ],
        detector_keywords: &[
            "oauth",
            "callback",
            "csrf",
            "cookie",
            "session",
            "login",
            "return_to",
            "redirect",
            "pkce",
            "nonce",
        ],
    },
    VulnerabilityClassDefinition {
        id: "ssrf_outbound_network",
        name: "SSRF and outbound network policy",
        description: "DNS rebinding, private/link-local/cloud metadata reachability, redirect-follow bypass, URL parser differentials, DNS pinning mistakes, and arbitrary header proxying.",
        examples: &[
            "SSRF",
            "DNS rebinding",
            "cloud metadata",
            "redirect-follow bypass",
            "URL parser differential",
            "address pinning",
            "proxy headers",
        ],
        evidence_requirements: &[
            "URL source",
            "outbound client/policy",
            "DNS/address validation point",
            "redirect/header behavior",
        ],
        detector_keywords: &[
            "http client",
            "reqwest",
            "url",
            "proxy",
            "metadata",
            "169.254",
            "dns",
            "redirect",
            "outbound",
        ],
    },
    VulnerabilityClassDefinition {
        id: "proxy_http_boundary",
        name: "Proxy and HTTP boundary issues",
        description: "Hop-by-hop header forwarding, request-smuggling assumptions, Host/X-Forwarded-* trust, CORS mistakes, auth-header forwarding across trust boundaries, response header injection, and content-type confusion.",
        examples: &[
            "hop-by-hop headers",
            "request smuggling",
            "Host header trust",
            "X-Forwarded",
            "CORS",
            "auth header forwarding",
            "response header injection",
            "content-type confusion",
        ],
        evidence_requirements: &[
            "proxy entry point",
            "forwarded header set",
            "upstream trust boundary",
            "response/header handling",
        ],
        detector_keywords: &[
            "proxy",
            "forward",
            "header",
            "host",
            "x-forwarded",
            "cors",
            "content-type",
            "upstream",
        ],
    },
    VulnerabilityClassDefinition {
        id: "container_sandbox_runtime",
        name: "Container, sandbox, and runtime escape",
        description: "Docker socket exposure, dangerous flags, entrypoint/command injection, PATH hijack, writable host mounts, capability/userns/cgroup/pid/ipc escape surfaces, gVisor/runsc fallback assumptions, and Unix socket IPC weaknesses.",
        examples: &[
            "Docker socket",
            "dangerous docker flags",
            "entrypoint injection",
            "PATH hijack",
            "host mounts",
            "privileged capabilities",
            "runsc fallback",
            "Unix socket IPC",
        ],
        evidence_requirements: &[
            "runtime launch path",
            "container args/env/mounts",
            "sandbox boundary",
            "caller identity or socket permission",
        ],
        detector_keywords: &[
            "docker",
            "container",
            "sandbox",
            "runtime",
            "runsc",
            "gvisor",
            "socket",
            "mount",
            "entrypoint",
        ],
    },
    VulnerabilityClassDefinition {
        id: "secrets_credentials",
        name: "Secrets and credential handling",
        description: "Plaintext storage, envelope/KMS context mistakes, cross-user or cross-instance reuse, inspect/log/audit/error exposure, env leakage, refresh-path leaks, and backup/snapshot secret handling.",
        examples: &[
            "plaintext secret storage",
            "KMS context mismatch",
            "cross-instance secret reuse",
            "secret in logs",
            "env leakage",
            "refresh token leak",
            "snapshot secrets",
        ],
        evidence_requirements: &[
            "secret source/storage",
            "encryption or envelope context",
            "owner/instance binding",
            "log/error/audit exposure path",
        ],
        detector_keywords: &[
            "secret",
            "credential",
            "kms",
            "envelope",
            "token",
            "password",
            "api key",
            "env",
            "snapshot",
        ],
    },
    VulnerabilityClassDefinition {
        id: "persistence_lifecycle",
        name: "Persistence, restore, clone, and lifecycle",
        description: "Secrets copied on clone, destroyed/paused instances still reachable, stale tokens after recreate/restore, state-file replay across tenants, migration ownership/encryption regressions, and backup/restore ownership confusion.",
        examples: &[
            "clone copies secrets",
            "destroyed instance reachable",
            "paused instance reachable",
            "stale token after restore",
            "state replay",
            "migration ownership confusion",
        ],
        evidence_requirements: &[
            "lifecycle operation",
            "state copied or restored",
            "owner/instance binding before and after",
            "reachability after status change",
        ],
        detector_keywords: &[
            "clone",
            "restore",
            "recreate",
            "delete",
            "destroy",
            "pause",
            "lifecycle",
            "migration",
            "backup",
            "state file",
        ],
    },
    VulnerabilityClassDefinition {
        id: "webhooks_inbound_integrations",
        name: "Webhooks and inbound integrations",
        description: "Signature verification flaws, timestamp/replay gaps, parser differentials, vendor header spoofing, path-token leakage, unauthenticated callbacks, and delivery body persistence exposure.",
        examples: &[
            "webhook signature",
            "timestamp replay",
            "vendor header spoofing",
            "path token leak",
            "unauthenticated callback",
            "delivery body exposure",
        ],
        evidence_requirements: &[
            "inbound integration route",
            "signature/timestamp validation",
            "vendor identity source",
            "body persistence or replay handling",
        ],
        detector_keywords: &[
            "webhook",
            "signature",
            "hmac",
            "callback",
            "integration",
            "delivery",
            "timestamp",
            "replay",
        ],
    },
    VulnerabilityClassDefinition {
        id: "file_archive_path",
        name: "File, archive, and path handling",
        description: "Path traversal, symlink traversal, archive extraction bugs, MIME/type confusion, unsafe file serving, public share path confusion, and artifact ID authorization bugs.",
        examples: &[
            "path traversal",
            "symlink traversal",
            "archive extraction",
            "MIME confusion",
            "unsafe file serving",
            "share path confusion",
            "artifact authorization",
        ],
        evidence_requirements: &[
            "file/path input source",
            "normalization/canonicalization",
            "authorization check",
            "file read/write/serve sink",
        ],
        detector_keywords: &[
            "file", "path", "archive", "zip", "tar", "symlink", "mime", "artifact", "share",
        ],
    },
    VulnerabilityClassDefinition {
        id: "injection_unsafe_execution",
        name: "Injection and unsafe execution",
        description: "Shell, SQL, NoSQL/query, template, deserialization, eval/dynamic import, regex DoS, and prompt/tool injection when it crosses a security boundary.",
        examples: &[
            "shell command injection",
            "SQL injection",
            "query injection",
            "template injection",
            "deserialization",
            "eval",
            "dynamic import",
            "regex DoS",
            "prompt/tool injection",
        ],
        evidence_requirements: &[
            "attacker-controlled source",
            "execution/query/template sink",
            "escaping/parameterization boundary",
            "runtime reachability",
        ],
        detector_keywords: &[
            "command",
            "shell",
            "sql",
            "query",
            "template",
            "deserialize",
            "eval",
            "regex",
            "prompt",
            "tool",
        ],
    },
    VulnerabilityClassDefinition {
        id: "dependency_supply_chain",
        name: "Dependency and supply chain",
        description: "Known CVEs, build script risks, unpinned images/actions, runtime image trust, registry/tag drift, typosquatting, and unexpected transitive tooling or postinstall hooks.",
        examples: &[
            "known CVEs",
            "build script risk",
            "unpinned image",
            "unpinned action",
            "runtime image trust",
            "tag drift",
            "typosquatting",
            "postinstall hook",
        ],
        evidence_requirements: &[
            "manifest/lockfile/image/action source",
            "scanner or registry evidence",
            "runtime/build reachability",
            "pinning or trust decision",
        ],
        detector_keywords: &[
            "cargo",
            "package",
            "lock",
            "npm",
            "pip",
            "dockerfile",
            "image",
            "github action",
            "dependency",
            "build",
        ],
    },
    VulnerabilityClassDefinition {
        id: "crypto_randomness",
        name: "Cryptography and randomness",
        description: "Weak randomness for tokens/state, nonce reuse, predictable IDs, incorrect hash/MAC use, insecure comparisons where entropy is low, and TLS downgrade/plaintext secret transit.",
        examples: &[
            "weak randomness",
            "nonce reuse",
            "predictable IDs",
            "hash misuse",
            "MAC misuse",
            "insecure compare",
            "TLS downgrade",
            "plaintext secrets",
        ],
        evidence_requirements: &[
            "security token or crypto primitive",
            "randomness/nonce source",
            "comparison/verification point",
            "transport/storage security decision",
        ],
        detector_keywords: &[
            "random", "rng", "nonce", "hash", "hmac", "crypto", "tls", "state", "pkce", "compare",
        ],
    },
    VulnerabilityClassDefinition {
        id: "multi_tenant_isolation",
        name: "Multi-tenant data isolation",
        description: "owner_id/instance_id mismatch, admin route leakage, list endpoints exposing other tenants, audit/log cross-tenant reads, cache keys missing tenant components, and shared runtime-helper confused deputy flaws.",
        examples: &[
            "owner_id mismatch",
            "instance_id mismatch",
            "admin route leakage",
            "cross-tenant list",
            "audit/log leak",
            "cache key tenant miss",
            "runtime helper confused deputy",
        ],
        evidence_requirements: &[
            "tenant/owner/instance source",
            "resource lookup",
            "authorization predicate",
            "list/cache/audit boundary",
        ],
        detector_keywords: &[
            "tenant",
            "owner",
            "instance",
            "admin",
            "audit",
            "cache",
            "runtime helper",
            "user_id",
        ],
    },
    VulnerabilityClassDefinition {
        id: "resource_exhaustion_dos",
        name: "Resource exhaustion and DoS",
        description: "Unbounded body reads, unbounded JSON parsing, streaming response caps, cache growth, per-instance rate-limit bypass, exposed expensive AST/query paths, and process/container leak cleanup failures.",
        examples: &[
            "unbounded body read",
            "unbounded JSON parse",
            "stream cap",
            "cache growth",
            "rate-limit bypass",
            "expensive query",
            "process leak",
            "container cleanup failure",
        ],
        evidence_requirements: &[
            "attacker-triggered entry point",
            "resource allocation or loop",
            "cap/timeout/rate limit",
            "cleanup path",
        ],
        detector_keywords: &[
            "body",
            "json",
            "stream",
            "cache",
            "rate",
            "limit",
            "timeout",
            "cleanup",
            "spawn",
            "container",
        ],
    },
    VulnerabilityClassDefinition {
        id: "frontend_security_ux",
        name: "Frontend and security UX",
        description: "Unsafe link rendering, dangerous markdown/html handling, secret reveal UX mistakes, share-link revocation gaps, clipboard/export leaks, and OAuth/login redirect UX abuse.",
        examples: &[
            "unsafe link rendering",
            "markdown/html handling",
            "secret reveal UX",
            "share-link revocation",
            "clipboard leak",
            "export leak",
            "redirect UX abuse",
        ],
        evidence_requirements: &[
            "frontend render or interaction point",
            "security-sensitive data/action",
            "sanitization/revocation/hide behavior",
            "browser/user trust boundary",
        ],
        detector_keywords: &[
            "frontend",
            "ui",
            "react",
            "markdown",
            "html",
            "clipboard",
            "share",
            "reveal",
            "oauth",
            "redirect",
        ],
    },
    VulnerabilityClassDefinition {
        id: "agent_tool_boundary",
        name: "Agent, MCP, and tool boundary",
        description: "Tool allowlist bypass, MCP server trust confusion, untrusted content steering privileged tools, synthetic tool-output injection, approval bypass, and agent identity confused-deputy paths.",
        examples: &[
            "tool allowlist bypass",
            "MCP trust confusion",
            "untrusted content steering tools",
            "synthetic tool-output injection",
            "approval bypass",
            "agent identity confused deputy",
        ],
        evidence_requirements: &[
            "agent/tool entry point",
            "tool authorization or allowlist decision",
            "untrusted instruction/data source",
            "privileged tool sink or identity boundary",
        ],
        detector_keywords: &[
            "agent",
            "tool",
            "mcp",
            "allowlist",
            "approval",
            "prompt",
            "instruction",
            "subagent",
            "identity",
        ],
    },
    VulnerabilityClassDefinition {
        id: "api_contract_input_validation",
        name: "API contract and input validation",
        description: "Schema drift, permissive defaults, enum fallthrough, partial update confusion, version downgrade behavior, malformed JSON/body handling, and inconsistent validation between clients, handlers, and workers.",
        examples: &[
            "schema drift",
            "default-permit input",
            "enum fallthrough",
            "partial update confusion",
            "version downgrade",
            "malformed JSON handling",
            "client/server validation mismatch",
        ],
        evidence_requirements: &[
            "request or message schema",
            "parser/deserializer behavior",
            "default or fallback branch",
            "security decision affected by invalid input",
        ],
        detector_keywords: &[
            "api",
            "mcp",
            "json-rpc",
            "request",
            "body",
            "schema",
            "json",
            "serde",
            "deserialize",
            "enum",
            "version",
            "validation",
            "fallback",
            "patch",
        ],
    },
    VulnerabilityClassDefinition {
        id: "audit_observability_forensics",
        name: "Audit, observability, and forensics",
        description: "Missing audit events for sensitive actions, audit identity spoofing, log integrity gaps, cross-tenant log disclosure, insufficient failure telemetry, and alert gaps that hide security-relevant state changes.",
        examples: &[
            "missing audit event",
            "audit identity spoofing",
            "log integrity gap",
            "cross-tenant log disclosure",
            "missing failure telemetry",
            "security alert gap",
        ],
        evidence_requirements: &[
            "sensitive action or failure path",
            "audit/log emission point",
            "actor and tenant attribution",
            "read access to audit/log record",
        ],
        detector_keywords: &[
            "audit",
            "log",
            "tracing",
            "telemetry",
            "event",
            "forensic",
            "alert",
            "actor",
            "history",
        ],
    },
    VulnerabilityClassDefinition {
        id: "ci_cd_release_integrity",
        name: "CI/CD and release integrity",
        description: "CI secret exposure, overbroad deploy tokens, unpinned build actions/images, unsigned artifacts, provenance gaps, branch protection bypass, release drift, and deployment script trust-boundary mistakes.",
        examples: &[
            "CI secret exposure",
            "overbroad deploy token",
            "unpinned action",
            "unsigned artifact",
            "missing provenance",
            "branch protection bypass",
            "release drift",
            "deployment script trust mistake",
        ],
        evidence_requirements: &[
            "workflow/build/deploy entry point",
            "secret or signing material scope",
            "artifact/image provenance",
            "promotion or deployment authorization decision",
        ],
        detector_keywords: &[
            "github action",
            "workflow",
            "ci",
            "deploy",
            "release",
            "artifact",
            "provenance",
            "signature",
            "token",
            "registry",
        ],
    },
    VulnerabilityClassDefinition {
        id: "data_retention_privacy",
        name: "Data retention and privacy",
        description: "PII exposure, retention/deletion mismatch, backup or export privacy leaks, stale shares after deletion, overbroad analytics capture, and privacy boundary drift between live state, snapshots, and logs.",
        examples: &[
            "PII exposure",
            "retention mismatch",
            "delete does not purge",
            "backup privacy leak",
            "export privacy leak",
            "stale share after deletion",
            "analytics overcapture",
        ],
        evidence_requirements: &[
            "sensitive data source",
            "retention/deletion/export path",
            "backup/snapshot/log copy behavior",
            "authorization or privacy boundary",
        ],
        detector_keywords: &[
            "pii",
            "privacy",
            "retention",
            "delete",
            "backup",
            "export",
            "snapshot",
            "share",
            "analytics",
        ],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VulnerabilityClassDefinition {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub examples: &'static [&'static str],
    pub evidence_requirements: &'static [&'static str],
    pub detector_keywords: &'static [&'static str],
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn vulnerability_taxonomy() -> &'static [VulnerabilityClassDefinition] {
    VULNERABILITY_TAXONOMY
}

/// Build the OrchestratorConfig for the security_engineer role.
pub fn security_engineer_config() -> OrchestratorConfig {
    OrchestratorConfig {
        name: "security_engineer",
        description: "Runs a staged, vulnerability-class-driven security research harness with \
             durable checkpoints: recon, taxonomy-based hunt batches, independent validation, \
             gapfill, dedupe, reachability tracing, feedback tasks, and schema-checked reporting. \
             Use for scoped authorized reviews and for resuming prior security_engineer \
             checkpoints.",
        system_prompt: include_str!("prompts/security_engineer.md"),
        direct_tool_names: DIRECT_TOOLS,
        // The staged harness uses smaller per-stage child budgets internally;
        // this remains the advertised ceiling for legacy metadata/tests and
        // as an upper bound for any single security stage child.
        max_iterations: 80,
        max_tokens: 8192,
        injects_protocol: Some(include_str!("prompts/security_engineer_protocol.md")),
        inject_cheatsheets: true,
        emit_artefact: Some(ArtefactKind::SecurityReview),
        harness: Some(OrchestratorHarness::SecurityResearch),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn harness_stages() -> &'static [SecurityHarnessStage] {
    STAGES
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SecurityHarnessStage {
    Recon,
    Hunt,
    Validate,
    Gapfill,
    Dedupe,
    Trace,
    Feedback,
    Report,
}

impl SecurityHarnessStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recon => "recon",
            Self::Hunt => "hunt",
            Self::Validate => "validate",
            Self::Gapfill => "gapfill",
            Self::Dedupe => "dedupe",
            Self::Trace => "trace",
            Self::Feedback => "feedback",
            Self::Report => "report",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "recon" => Some(Self::Recon),
            "hunt" => Some(Self::Hunt),
            "validate" => Some(Self::Validate),
            "gapfill" => Some(Self::Gapfill),
            "dedupe" => Some(Self::Dedupe),
            "trace" => Some(Self::Trace),
            "feedback" => Some(Self::Feedback),
            "report" => Some(Self::Report),
            _ => None,
        }
    }
}

impl fmt::Display for SecurityHarnessStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityTask {
    pub id: String,
    pub attack_class: String,
    pub scope_hint: String,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SecurityFinding {
    pub id: String,
    pub title: String,
    pub severity: String,
    #[serde(default)]
    pub vulnerability_class: String,
    #[serde(default)]
    pub trust_boundary: String,
    #[serde(default)]
    pub entry_point: String,
    #[serde(default)]
    pub sink_or_decision: String,
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub reachability: String,
    #[serde(default)]
    pub tenant_or_instance_impact: String,
    #[serde(default)]
    pub severity_rationale: String,
    #[serde(default)]
    pub fix_recommendation: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationDecisionKind {
    Confirmed,
    Rejected,
    NeedsMoreEvidence,
    Downgrade,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationDecision {
    pub finding_id: String,
    pub decision: ValidationDecisionKind,
    pub evidence: String,
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CoverageGap {
    pub area: String,
    pub reason: String,
    #[serde(default)]
    pub risk: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct VulnerabilityClassCoverage {
    pub class_id: String,
    pub class_name: String,
    pub considered: bool,
    pub applicable: bool,
    pub hunted: bool,
    #[serde(default)]
    pub skipped_reason: String,
    #[serde(default)]
    pub high_risk_follow_up: bool,
    #[serde(default)]
    pub checked_and_cleared: bool,
    #[serde(default)]
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DedupeGroup {
    pub id: String,
    #[serde(default)]
    pub root_cause: String,
    pub primary_finding_id: String,
    #[serde(default)]
    pub finding_ids: Vec<String>,
    #[serde(default)]
    pub affected_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TraceResult {
    pub finding_id: String,
    pub reachable: bool,
    pub severity_effect: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetRef {
    pub repo_path: String,
    #[serde(default)]
    pub git_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelMetadata {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub active_cheatsheets: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReportValidationState {
    pub status: String,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl Default for ReportValidationState {
    fn default() -> Self {
        Self {
            status: "not_started".into(),
            errors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageHistoryEntry {
    pub stage: SecurityHarnessStage,
    pub status: String,
    pub started_at: u64,
    pub finished_at: u64,
    #[serde(default)]
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityCheckpoint {
    pub schema_version: u32,
    pub harness_version: String,
    pub run_id: String,
    pub target: TargetRef,
    pub scope: String,
    pub current_stage: SecurityHarnessStage,
    #[serde(default)]
    pub architecture_context: String,
    #[serde(default)]
    pub completed_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub pending_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub findings_so_far: Vec<SecurityFinding>,
    #[serde(default)]
    pub validation_decisions_so_far: Vec<ValidationDecision>,
    #[serde(default)]
    pub dedupe_groups_so_far: Vec<DedupeGroup>,
    #[serde(default)]
    pub trace_results_so_far: Vec<TraceResult>,
    #[serde(default)]
    pub gapfill_tasks: Vec<SecurityTask>,
    #[serde(default)]
    pub coverage_gaps: Vec<CoverageGap>,
    #[serde(default)]
    pub class_coverage: Vec<VulnerabilityClassCoverage>,
    #[serde(default)]
    pub report_draft: Option<SecurityHarnessReport>,
    #[serde(default)]
    pub report_validation_state: ReportValidationState,
    #[serde(default)]
    pub stage_history: Vec<StageHistoryEntry>,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: ModelMetadata,
    #[serde(default)]
    pub completed: bool,
}

impl SecurityCheckpoint {
    pub fn new(
        run_id: String,
        target: TargetRef,
        scope: String,
        model: ModelMetadata,
        now: u64,
    ) -> Self {
        Self {
            schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
            harness_version: SECURITY_HARNESS_VERSION.into(),
            run_id,
            target,
            scope,
            current_stage: SecurityHarnessStage::Recon,
            architecture_context: String::new(),
            completed_tasks: Vec::new(),
            pending_tasks: Vec::new(),
            findings_so_far: Vec::new(),
            validation_decisions_so_far: Vec::new(),
            dedupe_groups_so_far: Vec::new(),
            trace_results_so_far: Vec::new(),
            gapfill_tasks: Vec::new(),
            coverage_gaps: Vec::new(),
            class_coverage: Vec::new(),
            report_draft: None,
            report_validation_state: ReportValidationState::default(),
            stage_history: Vec::new(),
            created_at: now,
            updated_at: now,
            model,
            completed: false,
        }
    }

    pub fn checkpoint_path(&self) -> String {
        checkpoint_path(&self.run_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecurityHarnessReport {
    pub schema_version: u32,
    pub run_id: String,
    pub target: TargetRef,
    pub scope: String,
    #[serde(default)]
    pub findings: Vec<SecurityFinding>,
    #[serde(default)]
    pub rejected_candidates: Vec<ValidationDecision>,
    #[serde(default)]
    pub coverage: Vec<CoverageGap>,
    #[serde(default)]
    pub gaps: Vec<CoverageGap>,
    #[serde(default)]
    pub dedupe_groups: Vec<DedupeGroup>,
    #[serde(default)]
    pub trace_evidence: Vec<TraceResult>,
    #[serde(default)]
    pub stage_history: Vec<StageHistoryEntry>,
    #[serde(default)]
    pub class_coverage: Vec<VulnerabilityClassCoverage>,
}

#[derive(Debug, Deserialize)]
struct ReconStageOutput {
    architecture_context: String,
    #[serde(default)]
    tasks: Vec<SecurityTask>,
    #[serde(default)]
    coverage_gaps: Vec<CoverageGap>,
    #[serde(default)]
    class_coverage: Vec<VulnerabilityClassCoverage>,
}

#[derive(Debug, Deserialize)]
struct HuntStageOutput {
    #[serde(default)]
    completed_task_ids: Vec<String>,
    #[serde(default)]
    findings: Vec<SecurityFinding>,
    #[serde(default)]
    gaps: Vec<CoverageGap>,
    #[serde(default)]
    follow_up_tasks: Vec<SecurityTask>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ValidateStageOutput {
    #[serde(default)]
    decisions: Vec<ValidationDecision>,
}

#[derive(Debug, Deserialize)]
struct TraceStageOutput {
    #[serde(default)]
    traces: Vec<TraceResult>,
}

pub(crate) struct SecurityHarnessRuntime {
    pub config_name: &'static str,
    pub provider: LlmProvider,
    pub model: String,
    pub client: RateLimitedHandle<Box<dyn LlmClient>>,
    pub sandbox: Arc<dyn Sandbox>,
    pub workspace: Option<WorkspaceHandle>,
    pub parent_depth: u8,
    pub scoped_dir: Option<PathBuf>,
    pub parent_working_dir: PathBuf,
    pub all_tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub user_message: String,
    pub parsed: OrchestratorInput,
    pub activity: Option<crate::controller::ActivityHandle>,
    pub events: Option<crate::controller::http::SubagentEventBus>,
    pub parent_tool_id: Option<String>,
    pub emit_artefact: Option<ArtefactKind>,
    pub active_sheets: Vec<String>,
    pub max_tokens: u32,
}

pub(crate) async fn run_security_harness(rt: SecurityHarnessRuntime) -> Result<ToolOutput> {
    let started_at = std::time::SystemTime::now();
    let started_epoch = unix_seconds(started_at);
    let mut activity_token = rt.activity.as_ref().map(|a| {
        a.start(
            crate::controller::LANE_SUBAGENT,
            rt.config_name,
            &crate::controller::truncate_note(&rt.user_message, 80),
        )
    });

    let result = run_security_harness_inner(&rt, started_epoch).await;

    if let Some(tok) = activity_token.take() {
        let elapsed = unix_seconds(std::time::SystemTime::now()).saturating_sub(started_epoch);
        let suffix = format!("{elapsed}s");
        let status = match &result {
            Ok(out) if !out.is_error => crate::controller::ActivityStatus::Ok,
            _ => crate::controller::ActivityStatus::Err,
        };
        tok.finish(status, Some(&suffix));
    }

    result
}

async fn run_security_harness_inner(
    rt: &SecurityHarnessRuntime,
    started_epoch: u64,
) -> Result<ToolOutput> {
    let store = CheckpointStore::new(rt.workspace.clone(), rt.parent_working_dir.clone());
    let target_path = rt
        .scoped_dir
        .as_deref()
        .unwrap_or(rt.parent_working_dir.as_path())
        .display()
        .to_string();
    let target = TargetRef {
        git_ref: git_ref_for(rt.scoped_dir.as_deref().unwrap_or(&rt.parent_working_dir)),
        repo_path: target_path,
    };
    let scope = scope_for(&rt.parsed);
    let model = ModelMetadata {
        provider: provider_label(&rt.provider),
        model: rt.model.clone(),
        active_cheatsheets: rt.active_sheets.clone(),
    };

    let resume_requested = rt.parsed.resume
        || rt
            .parsed
            .run_id
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
        || rt.parsed.task.to_ascii_lowercase().contains("resume");

    let mut checkpoint = if resume_requested {
        match load_checkpoint_for_resume(&store, rt.parsed.run_id.as_deref(), &target.repo_path)
            .await
        {
            Ok(cp) => cp,
            Err(e) => return Ok(ToolOutput::error(e)),
        }
    } else {
        SecurityCheckpoint::new(make_run_id(), target, scope, model, started_epoch)
    };

    if checkpoint.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Ok(ToolOutput::error(format!(
            "checkpoint {} uses unsupported schema_version {}; expected {}",
            checkpoint.run_id, checkpoint.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        )));
    }
    if checkpoint.completed {
        return Ok(ToolOutput::error(format!(
            "checkpoint {} is already complete",
            checkpoint.run_id
        )));
    }

    checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
    if let Err(e) = store.save(&checkpoint).await {
        return Ok(ToolOutput::error(e));
    }

    let mut out = ToolOutput::success(String::new());
    out.checkpoints.push(CheckpointEvent {
        message: format!(
            "security_engineer: {} checkpoint {}",
            if resume_requested {
                "resuming"
            } else {
                "created"
            },
            checkpoint.run_id
        ),
        progress: Some(0.02),
    });

    for stage in STAGES {
        if stage_completed(&checkpoint, *stage) {
            continue;
        }
        checkpoint.current_stage = *stage;
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        if let Err(e) = store.save(&checkpoint).await {
            return Ok(ToolOutput::error(e));
        }
        out.checkpoints.push(CheckpointEvent {
            message: format!("security_engineer: {stage}"),
            progress: progress_for(*stage),
        });

        let stage_started = unix_seconds(std::time::SystemTime::now());
        let stage_result = match stage {
            SecurityHarnessStage::Recon => run_recon_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Hunt => run_hunt_stage(rt, &store, &mut checkpoint).await,
            SecurityHarnessStage::Validate => run_validate_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Gapfill => {
                run_gapfill_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Dedupe => {
                run_dedupe_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Trace => run_trace_stage(rt, &mut checkpoint).await,
            SecurityHarnessStage::Feedback => {
                run_feedback_stage(&mut checkpoint);
                Ok(None)
            }
            SecurityHarnessStage::Report => run_report_stage(rt, &mut checkpoint).await,
        };

        match stage_result {
            Ok(Some(stage_output)) => merge_stage_tool_output(&mut out, stage_output),
            Ok(None) => {}
            Err(e) => {
                checkpoint.report_validation_state = ReportValidationState {
                    status: "failed".into(),
                    errors: vec![e.clone()],
                };
                checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
                let _ = store.save(&checkpoint).await;
                return Ok(ToolOutput::error(format!(
                    "security_engineer {stage} failed: {e}. checkpoint={}",
                    checkpoint.run_id
                )));
            }
        }

        let stage_finished = unix_seconds(std::time::SystemTime::now());
        checkpoint.stage_history.push(StageHistoryEntry {
            stage: *stage,
            status: "completed".into(),
            started_at: stage_started,
            finished_at: stage_finished,
            summary: stage_summary(&checkpoint, *stage),
        });
        checkpoint.updated_at = stage_finished;
        if let Err(e) = store.save(&checkpoint).await {
            return Ok(ToolOutput::error(e));
        }

        if should_stop_after(&rt.parsed, *stage) {
            out.content = format!(
                "security_engineer checkpoint saved after {stage}. run_id={} path={}. Resume with {{\"task\":\"resume security review\",\"resume\":true,\"run_id\":\"{}\"}}.",
                checkpoint.run_id,
                checkpoint.checkpoint_path(),
                checkpoint.run_id,
            );
            return Ok(out);
        }
    }

    checkpoint.completed = true;
    checkpoint.current_stage = SecurityHarnessStage::Report;
    checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
    if let Err(e) = store.save(&checkpoint).await {
        return Ok(ToolOutput::error(e));
    }

    let report = checkpoint
        .report_draft
        .clone()
        .unwrap_or_else(|| report_from_checkpoint(&checkpoint));
    let elapsed = checkpoint.updated_at.saturating_sub(started_epoch);
    out.content = render_report_markdown(&report, &checkpoint);
    out.checkpoints.push(CheckpointEvent {
        message: format!(
            "security_engineer: completed {} in {}s",
            checkpoint.run_id, elapsed
        ),
        progress: Some(1.0),
    });

    if let Some(kind) = rt.emit_artefact {
        let title = format!(
            "Security harness: {}",
            target_name_for(&report.target.repo_path)
        );
        let metadata = serde_json::json!({
            "run_id": checkpoint.run_id,
            "harness_version": SECURITY_HARNESS_VERSION,
            "schema_version": SECURITY_HARNESS_SCHEMA_VERSION,
            "target_path": report.target.repo_path,
            "provider": provider_label(&rt.provider),
            "model": rt.model,
            "checkpoint_path": checkpoint.checkpoint_path(),
            "stage_count": checkpoint.stage_history.len(),
        });
        out.artefacts
            .push(Artefact::markdown(kind, title, out.content.clone()).with_metadata(metadata));
    }

    Ok(out)
}

fn render_report_markdown(
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Security Harness Report: {}\n\n",
        report.target.repo_path
    ));
    out.push_str(&format!("- Run ID: `{}`\n", report.run_id));
    out.push_str(&format!("- Target: `{}`\n", report.target.repo_path));
    if let Some(git_ref) = &report.target.git_ref {
        out.push_str(&format!("- Git ref: `{git_ref}`\n"));
    }
    out.push_str(&format!(
        "- Checkpoint: `{}`\n",
        checkpoint.checkpoint_path()
    ));
    out.push_str(&format!(
        "- Report schema: `{}`\n\n",
        checkpoint.report_validation_state.status
    ));

    out.push_str("## Scope\n\n");
    out.push_str(&plain_block(&report.scope));
    out.push('\n');

    let confirmed = report.findings.len();
    let rejected = report.rejected_candidates.len();
    let reachable = report
        .trace_evidence
        .iter()
        .filter(|trace| trace.reachable)
        .count();
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Findings: {}\n", report.findings.len()));
    out.push_str(&format!("- Confirmed findings: {confirmed}\n"));
    out.push_str(&format!("- Rejected candidates: {rejected}\n"));
    out.push_str(&format!(
        "- Dedupe groups: {}\n",
        report.dedupe_groups.len()
    ));
    out.push_str(&format!(
        "- Reachable traces: {reachable}/{}\n",
        report.trace_evidence.len()
    ));
    out.push_str(&format!("- Coverage gaps: {}\n\n", report.gaps.len()));
    out.push_str(&format!(
        "- Vulnerability classes considered: {}\n",
        report.class_coverage.len()
    ));
    out.push_str(&format!(
        "- Vulnerability classes hunted: {}\n\n",
        report
            .class_coverage
            .iter()
            .filter(|class| class.hunted)
            .count()
    ));

    out.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        out.push_str("No confirmed findings were reported.\n\n");
    } else {
        for finding in &report.findings {
            render_finding_markdown(&mut out, report, checkpoint, finding);
        }
    }

    out.push_str("## Rejected Candidates\n\n");
    if report.rejected_candidates.is_empty() {
        out.push_str("No rejected candidates were recorded.\n\n");
    } else {
        for decision in &report.rejected_candidates {
            out.push_str(&format!(
                "### {}\n\n- Decision: `{}`\n- Evidence: {}\n\n",
                decision.finding_id,
                validation_decision_label(decision.decision),
                clean_inline(&decision.evidence)
            ));
        }
    }

    out.push_str("## Coverage And Gaps\n\n");
    if report.class_coverage.is_empty() {
        out.push_str("No vulnerability-class coverage accounting was recorded.\n\n");
    } else {
        out.push_str("### Vulnerability Classes\n\n");
        for class in &report.class_coverage {
            out.push_str(&format!(
                "- **{}** (`{}`): considered={} applicable={} hunted={} cleared={}",
                clean_inline(&class.class_name),
                clean_inline(&class.class_id),
                class.considered,
                class.applicable,
                class.hunted,
                class.checked_and_cleared
            ));
            if class.high_risk_follow_up {
                out.push_str(" follow_up=true");
            }
            if !class.skipped_reason.trim().is_empty() {
                out.push_str(&format!(" skipped={}", clean_inline(&class.skipped_reason)));
            }
            if !class.task_ids.is_empty() {
                out.push_str(&format!(" tasks={}", inline_code_list(&class.task_ids)));
            }
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("### Gaps\n\n");
    if report.gaps.is_empty() {
        out.push_str("No coverage gaps were recorded.\n\n");
    } else {
        for gap in &report.gaps {
            out.push_str(&format!(
                "- **{}** (`{}`): {}\n",
                clean_inline(&gap.area),
                if gap.risk.is_empty() {
                    "unknown"
                } else {
                    gap.risk.as_str()
                },
                clean_inline(&gap.reason)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Dedupe Groups\n\n");
    if report.dedupe_groups.is_empty() {
        out.push_str("No dedupe groups were recorded.\n\n");
    } else {
        for group in &report.dedupe_groups {
            out.push_str(&format!(
                "### {}\n\n- Primary finding: `{}`\n- Findings: {}\n- Root cause: {}\n",
                group.id,
                group.primary_finding_id,
                inline_code_list(&group.finding_ids),
                clean_inline(&group.root_cause)
            ));
            append_list(&mut out, "Affected paths", &group.affected_paths);
            out.push('\n');
        }
    }

    out.push_str("## Stage History\n\n");
    if report.stage_history.is_empty() {
        out.push_str("No stage history was recorded.\n");
    } else {
        for entry in &report.stage_history {
            out.push_str(&format!(
                "- `{}`: {} in {}s",
                entry.stage,
                entry.status,
                entry.finished_at.saturating_sub(entry.started_at)
            ));
            if !entry.summary.is_empty() {
                out.push_str(&format!(" - {}", clean_inline(&entry.summary)));
            }
            out.push('\n');
        }
    }

    out
}

fn render_finding_markdown(
    out: &mut String,
    report: &SecurityHarnessReport,
    checkpoint: &SecurityCheckpoint,
    finding: &SecurityFinding,
) {
    out.push_str(&format!("### {}: {}\n\n", finding.id, finding.title));
    out.push_str(&format!("- Severity: `{}`\n", finding.severity));
    out.push_str(&format!(
        "- Vulnerability class: `{}`\n",
        clean_inline(&finding.vulnerability_class)
    ));
    out.push_str(&format!(
        "- Trust boundary: {}\n",
        clean_inline(&finding.trust_boundary)
    ));
    out.push_str(&format!(
        "- Entry point: {}\n",
        clean_inline(&finding.entry_point)
    ));
    out.push_str(&format!(
        "- Sink/security decision: {}\n",
        clean_inline(&finding.sink_or_decision)
    ));
    if !finding.reachability.is_empty() {
        out.push_str(&format!(
            "- Reachability: `{}`\n",
            clean_inline(&finding.reachability)
        ));
    }
    out.push_str(&format!(
        "- Root cause: {}\n",
        clean_inline(&finding.root_cause)
    ));
    if !finding.tenant_or_instance_impact.trim().is_empty() {
        out.push_str(&format!(
            "- Tenant/instance impact: {}\n",
            clean_inline(&finding.tenant_or_instance_impact)
        ));
    }
    if !finding.severity_rationale.trim().is_empty() {
        out.push_str(&format!(
            "- Severity rationale: {}\n",
            clean_inline(&finding.severity_rationale)
        ));
    }
    if !finding.fix_recommendation.trim().is_empty() {
        out.push_str(&format!(
            "- Fix recommendation: {}\n",
            clean_inline(&finding.fix_recommendation)
        ));
    }

    if let Some(decision) = checkpoint
        .validation_decisions_so_far
        .iter()
        .find(|d| d.finding_id == finding.id)
    {
        out.push_str(&format!(
            "- Validation: `{}`",
            validation_decision_label(decision.decision)
        ));
        if let Some(severity) = &decision.severity {
            out.push_str(&format!(" as `{severity}`"));
        }
        out.push_str(&format!(" - {}\n", clean_inline(&decision.evidence)));
    }

    if let Some(trace) = report
        .trace_evidence
        .iter()
        .find(|trace| trace.finding_id == finding.id)
    {
        out.push_str(&format!(
            "- Trace: `{}`",
            if trace.reachable {
                "reachable"
            } else {
                "not reachable"
            }
        ));
        if !trace.severity_effect.is_empty() {
            out.push_str(&format!("; severity `{}`", trace.severity_effect));
        }
        out.push('\n');
        append_list(out, "Trace evidence", &trace.evidence);
    }

    if let Some(group) = report
        .dedupe_groups
        .iter()
        .find(|group| group.finding_ids.iter().any(|id| id == &finding.id))
    {
        out.push_str(&format!("- Dedupe group: `{}`\n", group.id));
    }

    append_list(out, "Affected paths", &finding.affected_paths);
    append_list(out, "Evidence", &finding.evidence);
    out.push('\n');
}

fn append_list(out: &mut String, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("\n{title}:\n"));
    for item in items {
        out.push_str(&format!("- {}\n", clean_inline(item)));
    }
}

fn plain_block(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "No explicit scope was recorded.\n".into();
    }
    trimmed
        .lines()
        .map(|line| format!("> {}\n", line.trim()))
        .collect()
}

fn clean_inline(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn inline_code_list(values: &[String]) -> String {
    if values.is_empty() {
        return "`none`".into();
    }
    values
        .iter()
        .map(|value| format!("`{}`", value))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validation_decision_label(decision: ValidationDecisionKind) -> &'static str {
    match decision {
        ValidationDecisionKind::Confirmed => "confirmed",
        ValidationDecisionKind::Rejected => "rejected",
        ValidationDecisionKind::NeedsMoreEvidence => "needs_more_evidence",
        ValidationDecisionKind::Downgrade => "downgrade",
    }
}

fn canonical_vulnerability_class(value: &str) -> Option<&'static str> {
    let normalized = normalize_class_id(value);
    for class in VULNERABILITY_TAXONOMY {
        if normalized == class.id {
            return Some(class.id);
        }
        if normalized == normalize_class_id(class.name) {
            return Some(class.id);
        }
        if class
            .examples
            .iter()
            .any(|example| normalized == normalize_class_id(example))
        {
            return Some(class.id);
        }
    }
    match normalized.as_str() {
        "auth_bypass" | "route_auth" | "authorization" | "idor" | "bola" | "tenant_boundary"
        | "role_confusion" | "bearer_leakage" => Some("auth_authorization"),
        "oauth" | "csrf" | "open_redirect" | "session" | "pkce" | "return_to" => {
            Some("session_oauth_csrf")
        }
        "ssrf" | "url_policy" | "network_policy" | "metadata_service" => {
            Some("ssrf_outbound_network")
        }
        "proxy" | "http_boundary" | "header_forwarding" | "cors" => Some("proxy_http_boundary"),
        "container" | "sandbox" | "runtime_escape" | "docker" | "docker_stdio" | "unix_socket" => {
            Some("container_sandbox_runtime")
        }
        "secrets" | "secret_handling" | "credentials" | "kms" | "envelope" => {
            Some("secrets_credentials")
        }
        "lifecycle" | "clone" | "restore" | "recreate" | "persistence" => {
            Some("persistence_lifecycle")
        }
        "webhooks" | "webhook" | "inbound_integrations" => Some("webhooks_inbound_integrations"),
        "path_traversal" | "files" | "file_serving" | "archive" | "artifact_auth" => {
            Some("file_archive_path")
        }
        "injection" | "command_injection" | "sql_injection" | "unsafe_execution"
        | "tool_injection" | "prompt_injection" => Some("injection_unsafe_execution"),
        "dependency" | "dependencies" | "supply_chain" | "dependency_review" => {
            Some("dependency_supply_chain")
        }
        "crypto" | "randomness" | "weak_rng" | "nonce_reuse" => Some("crypto_randomness"),
        "multi_tenant" | "tenant_isolation" | "owner_instance_mismatch" | "confused_deputy" => {
            Some("multi_tenant_isolation")
        }
        "dos" | "resource_exhaustion" | "rate_limit" | "body_cap" => {
            Some("resource_exhaustion_dos")
        }
        "frontend" | "security_ux" | "markdown" | "share_link" => Some("frontend_security_ux"),
        "agent" | "agentic" | "mcp" | "tool_boundary" | "tool_allowlist" | "approval_bypass"
        | "subagent" => Some("agent_tool_boundary"),
        "api_contract" | "input_validation" | "schema_validation" | "schema_drift"
        | "enum_fallthrough" | "malformed_json" => Some("api_contract_input_validation"),
        "audit" | "logging" | "observability" | "forensics" | "telemetry" => {
            Some("audit_observability_forensics")
        }
        "ci" | "cd" | "cicd" | "release_integrity" | "build_integrity" | "provenance" => {
            Some("ci_cd_release_integrity")
        }
        "privacy" | "data_retention" | "pii" | "retention" | "deletion" => {
            Some("data_retention_privacy")
        }
        _ => None,
    }
}

fn normalize_class_id(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn taxonomy_class(class_id: &str) -> Option<&'static VulnerabilityClassDefinition> {
    canonical_vulnerability_class(class_id)
        .and_then(|id| VULNERABILITY_TAXONOMY.iter().find(|class| class.id == id))
}

fn build_class_coverage(
    scope: &str,
    architecture_context: &str,
    provided: Vec<VulnerabilityClassCoverage>,
) -> Vec<VulnerabilityClassCoverage> {
    let context = format!("{scope}\n{architecture_context}").to_ascii_lowercase();
    let mut provided_by_id: BTreeMap<String, VulnerabilityClassCoverage> = BTreeMap::new();
    for mut item in provided {
        let Some(id) = canonical_vulnerability_class(&item.class_id) else {
            continue;
        };
        item.class_id = id.into();
        provided_by_id.insert(id.into(), item);
    }

    VULNERABILITY_TAXONOMY
        .iter()
        .map(|class| {
            let had_provided = provided_by_id.contains_key(class.id);
            let mut item = provided_by_id.remove(class.id).unwrap_or_default();
            let heuristic_applicable = class
                .detector_keywords
                .iter()
                .any(|keyword| context.contains(&keyword.to_ascii_lowercase()));
            item.class_id = class.id.into();
            item.class_name = class.name.into();
            item.considered = true;
            if !had_provided {
                item.applicable = heuristic_applicable;
            }
            if !item.applicable && item.skipped_reason.trim().is_empty() {
                item.skipped_reason =
                    "recon did not identify this class as applicable to the current scope".into();
            }
            item
        })
        .collect()
}

fn canonicalize_tasks(tasks: &mut [SecurityTask]) {
    for task in tasks {
        if let Some(class_id) = canonical_vulnerability_class(&task.attack_class) {
            task.attack_class = class_id.into();
        }
    }
}

fn canonicalize_findings(findings: &mut [SecurityFinding]) {
    for finding in findings {
        if let Some(class_id) = canonical_vulnerability_class(&finding.vulnerability_class) {
            finding.vulnerability_class = class_id.into();
        }
    }
}

fn ensure_taxonomy_hunt_tasks(checkpoint: &SecurityCheckpoint, tasks: &mut Vec<SecurityTask>) {
    let mut covered_classes: BTreeSet<&str> = tasks
        .iter()
        .filter_map(|task| canonical_vulnerability_class(&task.attack_class))
        .collect();
    let mut next = tasks.len() + 1;
    for coverage in &checkpoint.class_coverage {
        if !coverage.applicable || !coverage.skipped_reason.trim().is_empty() {
            continue;
        }
        let Some(class) = taxonomy_class(&coverage.class_id) else {
            continue;
        };
        if !covered_classes.insert(class.id) {
            continue;
        }
        tasks.push(SecurityTask {
            id: format!("hunt-{next:03}"),
            attack_class: class.id.into(),
            scope_hint: class_scope_hint(class, checkpoint),
            status: TaskStatus::Pending,
            rationale: format!(
                "{} Evidence must cover: {}.",
                class.description,
                class.evidence_requirements.join(", ")
            ),
        });
        next += 1;
    }
}

fn class_scope_hint(
    class: &VulnerabilityClassDefinition,
    checkpoint: &SecurityCheckpoint,
) -> String {
    let scope = checkpoint.scope.trim();
    if scope.is_empty() {
        format!(
            "{} across detected entry points and trust boundaries",
            class.name
        )
    } else {
        format!("{} within {}", class.name, clean_inline(scope))
    }
}

fn update_class_coverage_task_ids(
    coverage: &mut [VulnerabilityClassCoverage],
    tasks: &[SecurityTask],
) {
    for task in tasks {
        let Some(class_id) = canonical_vulnerability_class(&task.attack_class) else {
            continue;
        };
        if let Some(item) = coverage.iter_mut().find(|item| item.class_id == class_id)
            && !item.task_ids.iter().any(|id| id == &task.id)
        {
            item.task_ids.push(task.id.clone());
        }
    }
}

fn mark_hunted_class_coverage(
    coverage: &mut [VulnerabilityClassCoverage],
    completed_tasks: &[SecurityTask],
    findings: &[SecurityFinding],
) {
    let completed_classes: BTreeSet<&str> = completed_tasks
        .iter()
        .filter_map(|task| canonical_vulnerability_class(&task.attack_class))
        .collect();
    let finding_classes: BTreeSet<&str> = findings
        .iter()
        .filter_map(|finding| canonical_vulnerability_class(&finding.vulnerability_class))
        .collect();
    for item in coverage {
        if completed_classes.contains(item.class_id.as_str()) {
            item.hunted = true;
            item.checked_and_cleared = !finding_classes.contains(item.class_id.as_str());
        }
    }
}

fn missing_finding_evidence_fields(finding: &SecurityFinding) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if canonical_vulnerability_class(&finding.vulnerability_class).is_none() {
        missing.push("vulnerability_class");
    }
    if finding.trust_boundary.trim().is_empty() {
        missing.push("trust_boundary");
    }
    if finding.entry_point.trim().is_empty() {
        missing.push("entry_point");
    }
    if finding.sink_or_decision.trim().is_empty() {
        missing.push("sink_or_decision");
    }
    if finding.root_cause.trim().is_empty() {
        missing.push("root_cause");
    }
    if finding.evidence.is_empty() {
        missing.push("evidence");
    }
    if finding.severity_rationale.trim().is_empty() {
        missing.push("severity_rationale");
    }
    if finding.fix_recommendation.trim().is_empty() {
        missing.push("fix_recommendation");
    }
    missing
}

fn is_no_vulnerability_note(finding: &SecurityFinding) -> bool {
    let title = finding.title.to_ascii_lowercase();
    let root_cause = finding.root_cause.to_ascii_lowercase();
    let rationale = finding.severity_rationale.to_ascii_lowercase();
    let haystack = format!("{title}\n{root_cause}\n{rationale}");

    title.trim_start().starts_with("n/a")
        || haystack.contains("no vulnerability found")
        || haystack.contains("no bypass found")
        || haystack.contains("verified secure")
        || haystack.contains("verified safe")
        || haystack.contains("checked and cleared")
        || (title.contains("verified") && root_cause.contains("no vulnerability"))
        || (title.contains("verified") && rationale.contains("no vulnerability"))
}

pub(crate) fn reportable_confirmed_findings(
    checkpoint: &SecurityCheckpoint,
) -> Vec<&SecurityFinding> {
    let confirmed_ids: BTreeSet<&str> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|decision| decision.decision == ValidationDecisionKind::Confirmed)
        .map(|decision| decision.finding_id.as_str())
        .collect();
    checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| confirmed_ids.contains(finding.id.as_str()))
        .filter(|finding| missing_finding_evidence_fields(finding).is_empty())
        .filter(|finding| !is_no_vulnerability_note(finding))
        .collect()
}

fn reportable_finding_ids(checkpoint: &SecurityCheckpoint) -> BTreeSet<String> {
    reportable_confirmed_findings(checkpoint)
        .into_iter()
        .map(|finding| finding.id.clone())
        .collect()
}

fn report_checkpoint_for_prompt(checkpoint: &SecurityCheckpoint) -> SecurityCheckpoint {
    let mut filtered = checkpoint.clone();
    let reportable_ids = reportable_finding_ids(checkpoint);
    filtered.findings_so_far = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| reportable_ids.contains(&finding.id))
        .cloned()
        .collect();
    filtered.dedupe_groups_so_far = dedupe_findings(&filtered.findings_so_far);
    filtered.trace_results_so_far = checkpoint
        .trace_results_so_far
        .iter()
        .filter(|trace| reportable_ids.contains(&trace.finding_id))
        .cloned()
        .collect();
    filtered
}

async fn run_recon_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("prompts/security_engineer_recon.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Recon, prompt, checkpoint, 12).await?;
    let recon: ReconStageOutput = parse_stage_json(&raw)?;
    checkpoint.architecture_context = recon.architecture_context;
    checkpoint.class_coverage = build_class_coverage(
        &checkpoint.scope,
        &checkpoint.architecture_context,
        recon.class_coverage,
    );
    let mut tasks = recon.tasks;
    canonicalize_tasks(&mut tasks);
    ensure_taxonomy_hunt_tasks(checkpoint, &mut tasks);
    if tasks.is_empty() {
        tasks.push(SecurityTask {
            id: "hunt-001".into(),
            attack_class: "auth_authorization".into(),
            scope_hint: checkpoint.scope.clone(),
            status: TaskStatus::Pending,
            rationale: "fallback task because recon returned no tasks".into(),
        });
    }
    normalize_task_ids(&mut tasks, "hunt");
    update_class_coverage_task_ids(&mut checkpoint.class_coverage, &tasks);
    checkpoint.pending_tasks.extend(tasks);
    checkpoint.coverage_gaps.extend(recon.coverage_gaps);
    Ok(Some(stage_out))
}

async fn run_hunt_stage(
    rt: &SecurityHarnessRuntime,
    store: &CheckpointStore,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("prompts/security_engineer_hunt.md");
    let mut aggregate = ToolOutput::success(String::new());
    let mut ran_batch = false;

    for batch_index in 0..DEFAULT_MAX_HUNT_BATCHES {
        let batch = next_hunt_batch(checkpoint, DEFAULT_HUNT_BATCH_SIZE);
        if batch.is_empty() {
            break;
        }
        ran_batch = true;
        let mut checkpoint_for_prompt = checkpoint.clone();
        checkpoint_for_prompt.pending_tasks = batch.clone();
        let (raw, stage_out) = spawn_stage_with_checkpoint(
            rt,
            SecurityHarnessStage::Hunt,
            prompt,
            &checkpoint_for_prompt,
            28,
        )
        .await?;
        merge_stage_tool_output(&mut aggregate, stage_out);
        let hunt: HuntStageOutput = parse_stage_json(&raw)?;
        let completed_ids: BTreeSet<String> = if hunt.completed_task_ids.is_empty() {
            batch.iter().map(|t| t.id.clone()).collect()
        } else {
            hunt.completed_task_ids.into_iter().collect()
        };
        complete_tasks(checkpoint, &completed_ids);
        let mut findings = hunt
            .findings
            .into_iter()
            .filter(|finding| !finding.id.trim().is_empty())
            .collect::<Vec<_>>();
        canonicalize_findings(&mut findings);
        checkpoint.findings_so_far.extend(findings);
        checkpoint.coverage_gaps.extend(hunt.gaps);
        let mut followups = hunt.follow_up_tasks;
        canonicalize_tasks(&mut followups);
        normalize_task_ids(&mut followups, "gap");
        update_class_coverage_task_ids(&mut checkpoint.class_coverage, &followups);
        checkpoint.gapfill_tasks.extend(followups.clone());
        checkpoint.pending_tasks.extend(followups);
        mark_hunted_class_coverage(
            &mut checkpoint.class_coverage,
            &checkpoint.completed_tasks,
            &checkpoint.findings_so_far,
        );
        checkpoint.updated_at = unix_seconds(std::time::SystemTime::now());
        store.save(checkpoint).await?;
        aggregate.checkpoints.push(CheckpointEvent {
            message: format!(
                "security_engineer: hunt batch {} complete ({} completed, {} pending)",
                batch_index + 1,
                checkpoint.completed_tasks.len(),
                checkpoint
                    .pending_tasks
                    .iter()
                    .filter(|task| task.status == TaskStatus::Pending)
                    .count()
            ),
            progress: Some(0.25 + (batch_index as f32 * 0.04)),
        });
    }

    if ran_batch {
        Ok(Some(aggregate))
    } else {
        Ok(None)
    }
}

async fn run_validate_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    if checkpoint.findings_so_far.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("prompts/security_engineer_validate.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Validate, prompt, checkpoint, 16).await?;
    let validate = parse_validate_output(&raw, &checkpoint.findings_so_far)?;
    checkpoint
        .validation_decisions_so_far
        .extend(validate.decisions);
    Ok(Some(stage_out))
}

fn run_gapfill_stage(checkpoint: &mut SecurityCheckpoint) {
    let existing: BTreeSet<String> = checkpoint
        .pending_tasks
        .iter()
        .chain(checkpoint.completed_tasks.iter())
        .map(|t| t.id.clone())
        .collect();
    let mut next_id = checkpoint.pending_tasks.len() + checkpoint.completed_tasks.len() + 1;
    let mut additions = Vec::new();
    for gap in &checkpoint.coverage_gaps {
        let risk = gap.risk.to_ascii_lowercase();
        if !(risk.contains("high") || risk.contains("critical")) {
            continue;
        }
        let id = format!("gapfill-{next_id:03}");
        next_id += 1;
        if existing.contains(&id) {
            continue;
        }
        additions.push(SecurityTask {
            id,
            attack_class: canonical_vulnerability_class(&gap.area)
                .unwrap_or("resource_exhaustion_dos")
                .into(),
            scope_hint: gap.area.clone(),
            status: TaskStatus::Pending,
            rationale: gap.reason.clone(),
        });
    }
    update_class_coverage_task_ids(&mut checkpoint.class_coverage, &additions);
    checkpoint.gapfill_tasks.extend(additions.clone());
    checkpoint.pending_tasks.extend(additions);
}

pub(crate) fn run_dedupe_stage(checkpoint: &mut SecurityCheckpoint) {
    let findings = reportable_confirmed_findings(checkpoint)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    checkpoint.dedupe_groups_so_far = dedupe_findings(&findings);
}

async fn run_trace_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let confirmed: Vec<&ValidationDecision> = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Confirmed)
        .collect();
    if confirmed.is_empty() {
        return Ok(None);
    }
    let prompt = include_str!("prompts/security_engineer_trace.md");
    let (raw, stage_out) =
        spawn_stage(rt, SecurityHarnessStage::Trace, prompt, checkpoint, 16).await?;
    match parse_stage_json::<TraceStageOutput>(&raw) {
        Ok(traces) => {
            checkpoint.trace_results_so_far.extend(traces.traces);
        }
        Err(err) => {
            checkpoint.coverage_gaps.push(CoverageGap {
                area: "Trace stage".into(),
                reason: format!(
                    "Trace stage output was not parseable JSON; continuing with existing reachability evidence: {err}"
                ),
                risk: "unknown".into(),
            });
            checkpoint.report_validation_state = ReportValidationState {
                status: "trace_unparsed".into(),
                errors: vec![err],
            };
        }
    }
    Ok(Some(stage_out))
}

fn run_feedback_stage(checkpoint: &mut SecurityCheckpoint) {
    let mut next = checkpoint.pending_tasks.len() + checkpoint.completed_tasks.len() + 1;
    let mut existing: BTreeSet<String> = checkpoint
        .pending_tasks
        .iter()
        .chain(checkpoint.completed_tasks.iter())
        .map(|t| t.scope_hint.clone())
        .collect();
    for trace in &checkpoint.trace_results_so_far {
        if !trace.reachable {
            continue;
        }
        let Some(finding) = checkpoint
            .findings_so_far
            .iter()
            .find(|finding| finding.id == trace.finding_id)
        else {
            continue;
        };
        for path in &finding.affected_paths {
            if !existing.insert(path.clone()) {
                continue;
            }
            checkpoint.pending_tasks.push(SecurityTask {
                id: format!("feedback-{next:03}"),
                attack_class: canonical_vulnerability_class(&finding.vulnerability_class)
                    .unwrap_or("injection_unsafe_execution")
                    .into(),
                scope_hint: path.clone(),
                status: TaskStatus::Pending,
                rationale: "reachable shared-component finding; inspect consumer path".into(),
            });
            if let Some(task) = checkpoint.pending_tasks.last() {
                update_class_coverage_task_ids(
                    &mut checkpoint.class_coverage,
                    std::slice::from_ref(task),
                );
            }
            next += 1;
        }
    }
}

async fn run_report_stage(
    rt: &SecurityHarnessRuntime,
    checkpoint: &mut SecurityCheckpoint,
) -> std::result::Result<Option<ToolOutput>, String> {
    let prompt = include_str!("prompts/security_engineer_report.md");
    let checkpoint_for_prompt = report_checkpoint_for_prompt(checkpoint);
    let (raw, mut stage_out) = spawn_stage(
        rt,
        SecurityHarnessStage::Report,
        prompt,
        &checkpoint_for_prompt,
        10,
    )
    .await?;
    let (parsed, validation_state) = match parse_report_output(&raw) {
        Ok(report) => (
            report,
            ReportValidationState {
                status: "valid".into(),
                errors: Vec::new(),
            },
        ),
        Err(first_err) => {
            checkpoint.report_validation_state = ReportValidationState {
                status: "repairing".into(),
                errors: vec![first_err.clone()],
            };
            let repair_prompt = include_str!("prompts/security_engineer_report_repair.md");
            let mut repair_checkpoint = checkpoint_for_prompt.clone();
            repair_checkpoint.report_validation_state = checkpoint.report_validation_state.clone();
            let (repair_raw, repair_out) = spawn_stage(
                rt,
                SecurityHarnessStage::Report,
                repair_prompt,
                &repair_checkpoint,
                6,
            )
            .await?;
            merge_stage_tool_output(&mut stage_out, repair_out);
            resolve_repaired_or_fallback_report(checkpoint, &first_err, &repair_raw)?
        }
    };
    checkpoint.report_validation_state = validation_state;
    checkpoint.report_draft = Some(parsed);
    Ok(Some(stage_out))
}

pub(crate) fn resolve_repaired_or_fallback_report(
    checkpoint: &SecurityCheckpoint,
    first_err: &str,
    repair_raw: &str,
) -> std::result::Result<(SecurityHarnessReport, ReportValidationState), String> {
    match parse_report_output(repair_raw) {
        Ok(report) => Ok((
            report,
            ReportValidationState {
                status: "valid".into(),
                errors: Vec::new(),
            },
        )),
        Err(second_err) => {
            let fallback = report_from_checkpoint(checkpoint);
            let fallback_value = serde_json::to_value(&fallback)
                .map_err(|e| format!("serialize deterministic report fallback: {e}"))?;
            match validate_report_json(&fallback_value) {
                Ok(report) => Ok((
                    report,
                    ReportValidationState {
                        status: "deterministic_fallback".into(),
                        errors: vec![first_err.to_string(), second_err],
                    },
                )),
                Err(fallback_err) => Err(format!(
                    "report schema validation failed after repair: {first_err}; {second_err}; deterministic fallback failed: {fallback_err}"
                )),
            }
        }
    }
}

async fn spawn_stage(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    checkpoint: &SecurityCheckpoint,
    max_iterations: usize,
) -> std::result::Result<(String, ToolOutput), String> {
    spawn_stage_with_checkpoint(rt, stage, prompt, checkpoint, max_iterations).await
}

async fn spawn_stage_with_checkpoint(
    rt: &SecurityHarnessRuntime,
    stage: SecurityHarnessStage,
    prompt: &str,
    checkpoint: &SecurityCheckpoint,
    max_iterations: usize,
) -> std::result::Result<(String, ToolOutput), String> {
    let checkpoint_json = serde_json::to_string_pretty(checkpoint)
        .map_err(|e| format!("serialize checkpoint for {stage}: {e}"))?;
    let system_prompt = format!("{}\n\n{}", rt.system_prompt, prompt);
    let stage_message = format!(
        "Parent request:\n{}\n\nCurrent durable checkpoint JSON:\n```json\n{}\n```\n",
        rt.user_message, checkpoint_json
    );
    let settings = AgentSettings {
        model: rt.model.clone(),
        max_iterations,
        max_tokens: rt.max_tokens,
        system_prompt,
        provider: rt.provider.clone(),
        ..AgentSettings::default()
    };
    let out = spawn_child(ChildSpawn {
        name: stage.as_str(),
        settings,
        inherited_tools: rt.all_tools.clone(),
        sandbox: Arc::clone(&rt.sandbox),
        workspace: rt.workspace.clone(),
        client: rt.client.clone(),
        parent_depth: rt.parent_depth,
        working_dir: rt.scoped_dir.clone(),
        user_message: stage_message,
        activity: rt.activity.clone(),
        events: rt.events.clone(),
        parent_tool_id: rt.parent_tool_id.clone(),
    })
    .await
    .map_err(|e| e.to_string())?;
    if out.is_error {
        return Err(out.content);
    }
    Ok((out.content.clone(), out))
}

fn merge_stage_tool_output(target: &mut ToolOutput, mut stage: ToolOutput) {
    target.checkpoints.append(&mut stage.checkpoints);
    target.artefacts.append(&mut stage.artefacts);
    let Some(stage_meta) = stage.metadata.take() else {
        return;
    };
    let mut meta = target.metadata.take().unwrap_or_else(|| {
        serde_json::json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "llm_calls": 0,
        })
    });
    for key in ["input_tokens", "output_tokens", "llm_calls"] {
        let current = meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let add = stage_meta.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        meta[key] = serde_json::json!(current + add);
    }
    target.metadata = Some(meta);
}

pub fn validate_report_json(
    value: &serde_json::Value,
) -> std::result::Result<SecurityHarnessReport, String> {
    prevalidate_report_value(value)?;
    let report: SecurityHarnessReport =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    validate_report_struct(report)
}

fn validate_report_struct(
    report: SecurityHarnessReport,
) -> std::result::Result<SecurityHarnessReport, String> {
    if report.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema_version {}; expected {}",
            report.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        ));
    }
    if report.run_id.trim().is_empty() {
        return Err("run_id is required".into());
    }
    if report.target.repo_path.trim().is_empty() {
        return Err("target.repo_path is required".into());
    }
    if report.class_coverage.is_empty() {
        return Err("class_coverage is required".into());
    }
    for finding in &report.findings {
        if finding.id.trim().is_empty()
            || finding.title.trim().is_empty()
            || finding.root_cause.trim().is_empty()
        {
            return Err("findings require id, title, and root_cause".into());
        }
        let missing = missing_finding_evidence_fields(finding);
        if !missing.is_empty() {
            return Err(format!(
                "finding {} missing required evidence fields: {}",
                finding.id,
                missing.join(", ")
            ));
        }
    }
    for (idx, group) in report.dedupe_groups.iter().enumerate() {
        if group.id.trim().is_empty()
            || group.primary_finding_id.trim().is_empty()
            || group.finding_ids.is_empty()
            || group.root_cause.trim().is_empty()
        {
            return Err(format!(
                "dedupe_groups[{idx}] {} requires id, primary_finding_id, finding_ids, and root_cause",
                describe_dedupe_group(group)
            ));
        }
    }
    Ok(report)
}

fn prevalidate_report_value(value: &serde_json::Value) -> std::result::Result<(), String> {
    if let Some(findings) = value.get("findings").and_then(|v| v.as_array()) {
        for (idx, finding) in findings.iter().enumerate() {
            if missing_or_empty_string(finding.get("root_cause")) {
                return Err(format!(
                    "findings[{idx}] {} missing required field root_cause",
                    describe_value_item(finding, &["id", "title"])
                ));
            }
        }
    }
    if let Some(groups) = value.get("dedupe_groups").and_then(|v| v.as_array()) {
        for (idx, group) in groups.iter().enumerate() {
            if missing_or_empty_string(group.get("root_cause")) {
                return Err(format!(
                    "dedupe_groups[{idx}] {} missing required field root_cause",
                    describe_value_item(group, &["id", "primary_finding_id"])
                ));
            }
        }
    }
    Ok(())
}

fn missing_or_empty_string(value: Option<&serde_json::Value>) -> bool {
    match value.and_then(|v| v.as_str()) {
        Some(s) => s.trim().is_empty(),
        None => true,
    }
}

fn describe_value_item(value: &serde_json::Value, keys: &[&str]) -> String {
    let details = keys
        .iter()
        .filter_map(|key| value.get(*key).and_then(|v| v.as_str()).map(|v| (*key, v)))
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(key, value)| format!("{key}={}", clean_inline(value)))
        .collect::<Vec<_>>();
    if details.is_empty() {
        "(unidentified item)".into()
    } else {
        format!("({})", details.join(" "))
    }
}

fn describe_dedupe_group(group: &DedupeGroup) -> String {
    let mut details = Vec::new();
    if !group.id.trim().is_empty() {
        details.push(format!("id={}", clean_inline(&group.id)));
    }
    if !group.primary_finding_id.trim().is_empty() {
        details.push(format!(
            "primary_finding_id={}",
            clean_inline(&group.primary_finding_id)
        ));
    }
    if details.is_empty() {
        "(unidentified item)".into()
    } else {
        format!("({})", details.join(" "))
    }
}

pub(crate) fn parse_report_output(raw: &str) -> std::result::Result<SecurityHarnessReport, String> {
    let value = parse_json_value(raw)?;
    validate_report_json(&value)
}

pub(crate) fn parse_validate_output(
    raw: &str,
    findings: &[SecurityFinding],
) -> std::result::Result<ValidateStageOutput, String> {
    let value = parse_json_value(raw)?;
    if value.get("findings").is_some() {
        return Err("validator output must not include new findings".into());
    }
    let parsed: ValidateStageOutput = serde_json::from_value(value).map_err(|e| e.to_string())?;
    let known: BTreeSet<&str> = findings.iter().map(|f| f.id.as_str()).collect();
    for decision in &parsed.decisions {
        if !known.contains(decision.finding_id.as_str()) {
            return Err(format!(
                "validator referenced unknown finding_id {}",
                decision.finding_id
            ));
        }
        if decision.decision == ValidationDecisionKind::Confirmed {
            if decision.evidence.trim().is_empty() {
                return Err(format!(
                    "validator confirmation for {} requires evidence",
                    decision.finding_id
                ));
            }
            if let Some(finding) = findings.iter().find(|f| f.id == decision.finding_id) {
                let missing = missing_finding_evidence_fields(finding);
                if !missing.is_empty() {
                    return Err(format!(
                        "validator cannot confirm {} without required finding fields: {}",
                        decision.finding_id,
                        missing.join(", ")
                    ));
                }
                if is_no_vulnerability_note(finding) {
                    return Err(format!(
                        "validator cannot confirm {} because it is a no-vulnerability verification note, not a reportable finding",
                        decision.finding_id
                    ));
                }
            }
        }
    }
    Ok(parsed)
}

fn parse_stage_json<T: for<'de> Deserialize<'de>>(raw: &str) -> std::result::Result<T, String> {
    let value = parse_json_value(raw)?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn parse_json_value(raw: &str) -> std::result::Result<serde_json::Value, String> {
    let candidate =
        extract_json(raw).ok_or_else(|| "no JSON object found in stage output".to_string())?;
    serde_json::from_str(candidate).map_err(|e| format!("invalid JSON: {e}"))
}

fn extract_json(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + "```json".len()..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim());
        }
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end > start).then(|| trimmed[start..=end].trim())
}

fn next_hunt_batch(checkpoint: &SecurityCheckpoint, batch_size: usize) -> Vec<SecurityTask> {
    checkpoint
        .pending_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Pending)
        .take(batch_size)
        .cloned()
        .collect()
}

fn complete_tasks(checkpoint: &mut SecurityCheckpoint, completed_ids: &BTreeSet<String>) {
    let mut remaining = Vec::new();
    for mut task in checkpoint.pending_tasks.drain(..) {
        if completed_ids.contains(&task.id) {
            task.status = TaskStatus::Completed;
            checkpoint.completed_tasks.push(task);
        } else {
            remaining.push(task);
        }
    }
    checkpoint.pending_tasks = remaining;
}

fn normalize_task_ids(tasks: &mut [SecurityTask], prefix: &str) {
    for (idx, task) in tasks.iter_mut().enumerate() {
        if task.id.trim().is_empty() {
            task.id = format!("{prefix}-{:03}", idx + 1);
        }
    }
}

pub fn dedupe_findings(findings: &[SecurityFinding]) -> Vec<DedupeGroup> {
    let mut by_root: BTreeMap<String, Vec<&SecurityFinding>> = BTreeMap::new();
    for finding in findings {
        let root = if finding.root_cause.trim().is_empty() {
            finding.title.clone()
        } else {
            finding.root_cause.clone()
        };
        by_root.entry(root).or_default().push(finding);
    }
    by_root
        .into_iter()
        .enumerate()
        .map(|(idx, (root_cause, group))| {
            let primary = group.first().map(|f| f.id.clone()).unwrap_or_default();
            let mut affected = BTreeSet::new();
            for finding in &group {
                affected.extend(finding.affected_paths.iter().cloned());
            }
            DedupeGroup {
                id: format!("dedupe-{:03}", idx + 1),
                root_cause,
                primary_finding_id: primary,
                finding_ids: group.iter().map(|f| f.id.clone()).collect(),
                affected_paths: affected.into_iter().collect(),
            }
        })
        .collect()
}

pub(crate) fn report_from_checkpoint(checkpoint: &SecurityCheckpoint) -> SecurityHarnessReport {
    let reportable_ids = reportable_finding_ids(checkpoint);
    let findings = checkpoint
        .findings_so_far
        .iter()
        .filter(|finding| reportable_ids.contains(&finding.id))
        .cloned()
        .collect::<Vec<_>>();
    let rejected_candidates = checkpoint
        .validation_decisions_so_far
        .iter()
        .filter(|d| d.decision == ValidationDecisionKind::Rejected)
        .cloned()
        .collect();
    let trace_evidence = checkpoint
        .trace_results_so_far
        .iter()
        .filter(|trace| reportable_ids.contains(&trace.finding_id))
        .cloned()
        .collect();
    SecurityHarnessReport {
        schema_version: SECURITY_HARNESS_SCHEMA_VERSION,
        run_id: checkpoint.run_id.clone(),
        target: checkpoint.target.clone(),
        scope: checkpoint.scope.clone(),
        findings: findings.clone(),
        rejected_candidates,
        coverage: checkpoint.coverage_gaps.clone(),
        gaps: checkpoint.coverage_gaps.clone(),
        dedupe_groups: dedupe_findings(&findings),
        trace_evidence,
        stage_history: checkpoint.stage_history.clone(),
        class_coverage: checkpoint.class_coverage.clone(),
    }
}

fn stage_completed(checkpoint: &SecurityCheckpoint, stage: SecurityHarnessStage) -> bool {
    checkpoint
        .stage_history
        .iter()
        .any(|entry| entry.stage == stage && entry.status == "completed")
}

fn stage_summary(checkpoint: &SecurityCheckpoint, stage: SecurityHarnessStage) -> String {
    match stage {
        SecurityHarnessStage::Recon => format!(
            "{} pending hunt tasks; {} applicable classes; {} gaps",
            checkpoint.pending_tasks.len(),
            checkpoint
                .class_coverage
                .iter()
                .filter(|class| class.applicable)
                .count(),
            checkpoint.coverage_gaps.len()
        ),
        SecurityHarnessStage::Hunt => format!(
            "{} completed tasks; {} findings",
            checkpoint.completed_tasks.len(),
            checkpoint.findings_so_far.len()
        ),
        SecurityHarnessStage::Validate => {
            format!(
                "{} validation decisions",
                checkpoint.validation_decisions_so_far.len()
            )
        }
        SecurityHarnessStage::Gapfill => {
            format!("{} gapfill tasks", checkpoint.gapfill_tasks.len())
        }
        SecurityHarnessStage::Dedupe => {
            format!("{} dedupe groups", checkpoint.dedupe_groups_so_far.len())
        }
        SecurityHarnessStage::Trace => {
            format!("{} trace results", checkpoint.trace_results_so_far.len())
        }
        SecurityHarnessStage::Feedback => {
            format!("{} pending feedback tasks", checkpoint.pending_tasks.len())
        }
        SecurityHarnessStage::Report => checkpoint.report_validation_state.status.clone(),
    }
}

fn progress_for(stage: SecurityHarnessStage) -> Option<f32> {
    Some(match stage {
        SecurityHarnessStage::Recon => 0.10,
        SecurityHarnessStage::Hunt => 0.25,
        SecurityHarnessStage::Validate => 0.45,
        SecurityHarnessStage::Gapfill => 0.58,
        SecurityHarnessStage::Dedupe => 0.66,
        SecurityHarnessStage::Trace => 0.76,
        SecurityHarnessStage::Feedback => 0.86,
        SecurityHarnessStage::Report => 0.94,
    })
}

fn should_stop_after(parsed: &OrchestratorInput, stage: SecurityHarnessStage) -> bool {
    parsed
        .stop_after_stage
        .as_deref()
        .and_then(SecurityHarnessStage::parse)
        == Some(stage)
}

fn scope_for(parsed: &OrchestratorInput) -> String {
    let mut parts = Vec::new();
    if !parsed.path.trim().is_empty() {
        parts.push(format!("path={}", parsed.path.trim()));
    }
    if !parsed.context.trim().is_empty() {
        parts.push(format!("context={}", parsed.context.trim()));
    }
    parts.push(format!("task={}", parsed.task.trim()));
    parts.join("\n")
}

fn make_run_id() -> String {
    format!(
        "sec-{}-{}",
        unix_seconds(std::time::SystemTime::now()),
        std::process::id()
    )
}

fn checkpoint_path(run_id: &str) -> String {
    format!("{CHECKPOINT_PREFIX}/{run_id}.json")
}

fn unix_seconds(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn provider_label(provider: &LlmProvider) -> String {
    format!("{provider:?}")
}

fn target_name_for(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("target")
        .to_string()
}

fn git_ref_for(path: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

struct CheckpointStore {
    workspace: Option<WorkspaceHandle>,
    fallback_dir: PathBuf,
}

impl CheckpointStore {
    fn new(workspace: Option<WorkspaceHandle>, working_dir: PathBuf) -> Self {
        Self {
            workspace,
            fallback_dir: working_dir
                .join(".dyson")
                .join("security-harness")
                .join("checkpoints"),
        }
    }

    async fn save(&self, checkpoint: &SecurityCheckpoint) -> std::result::Result<(), String> {
        let body = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
        if let Some(workspace) = &self.workspace {
            let mut guard = workspace.write().await;
            guard.set(&checkpoint.checkpoint_path(), &body);
            guard.save().map_err(|e| e.to_string())?;
            return Ok(());
        }
        std::fs::create_dir_all(&self.fallback_dir).map_err(|e| {
            format!(
                "cannot create checkpoint dir {}: {e}",
                self.fallback_dir.display()
            )
        })?;
        std::fs::write(
            self.fallback_dir
                .join(format!("{}.json", checkpoint.run_id)),
            body,
        )
        .map_err(|e| format!("cannot write checkpoint: {e}"))
    }

    async fn load_exact(&self, run_id: &str) -> std::result::Result<SecurityCheckpoint, String> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let path = checkpoint_path(run_id);
            let Some(body) = guard.get(&path) else {
                let disk_root = guard
                    .programs_dir()
                    .and_then(|programs| programs.parent().map(std::path::Path::to_path_buf));
                drop(guard);
                if let Some(root) = disk_root {
                    let path = root.join(checkpoint_path(run_id));
                    let body = std::fs::read_to_string(&path).map_err(|_| {
                        format!("checkpoint {run_id} not found at {}", path.display())
                    })?;
                    return parse_checkpoint(&body);
                }
                return Err(format!("checkpoint {run_id} not found"));
            };
            return parse_checkpoint(&body);
        }
        let path = self.fallback_dir.join(format!("{run_id}.json"));
        let body = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read checkpoint {}: {e}", path.display()))?;
        parse_checkpoint(&body)
    }

    async fn list(&self) -> Vec<SecurityCheckpoint> {
        if let Some(workspace) = &self.workspace {
            let guard = workspace.read().await;
            let mut checkpoints: Vec<SecurityCheckpoint> = guard
                .list_files()
                .into_iter()
                .filter(|p| p.starts_with(CHECKPOINT_PREFIX) && p.ends_with(".json"))
                .filter_map(|p| guard.get(&p).and_then(|body| parse_checkpoint(&body).ok()))
                .collect();
            let disk_root = guard
                .programs_dir()
                .and_then(|programs| programs.parent().map(std::path::Path::to_path_buf));
            drop(guard);
            if let Some(root) = disk_root {
                checkpoints.extend(read_checkpoint_dir(root.join(CHECKPOINT_PREFIX)));
                checkpoints.sort_by(|a, b| a.run_id.cmp(&b.run_id));
                checkpoints.dedup_by(|a, b| a.run_id == b.run_id);
            }
            return checkpoints;
        }
        read_checkpoint_dir(self.fallback_dir.clone())
    }
}

fn read_checkpoint_dir(path: PathBuf) -> Vec<SecurityCheckpoint> {
    let Ok(entries) = std::fs::read_dir(&path) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            let body = std::fs::read_to_string(path).ok()?;
            parse_checkpoint(&body).ok()
        })
        .collect()
}

fn parse_checkpoint(body: &str) -> std::result::Result<SecurityCheckpoint, String> {
    let checkpoint: SecurityCheckpoint = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if checkpoint.schema_version != SECURITY_HARNESS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported checkpoint schema_version {}; expected {}",
            checkpoint.schema_version, SECURITY_HARNESS_SCHEMA_VERSION
        ));
    }
    if checkpoint.harness_version != SECURITY_HARNESS_VERSION {
        return Err(format!(
            "unsupported checkpoint harness_version {}; expected {}",
            checkpoint.harness_version, SECURITY_HARNESS_VERSION
        ));
    }
    Ok(checkpoint)
}

async fn load_checkpoint_for_resume(
    store: &CheckpointStore,
    run_id: Option<&str>,
    target_path: &str,
) -> std::result::Result<SecurityCheckpoint, String> {
    if let Some(run_id) = run_id.filter(|s| !s.trim().is_empty()) {
        return store.load_exact(run_id.trim()).await;
    }
    let mut matches: Vec<SecurityCheckpoint> = store
        .list()
        .await
        .into_iter()
        .filter(|cp| !cp.completed && cp.target.repo_path == target_path)
        .collect();
    matches.sort_by_key(|cp| std::cmp::Reverse(cp.updated_at));
    match matches.len() {
        0 => Err(format!(
            "no incomplete security_engineer checkpoint found for {target_path}"
        )),
        1 => Ok(matches.remove(0)),
        _ => {
            let list = matches
                .iter()
                .take(8)
                .map(|cp| {
                    format!(
                        "- {} stage={} updated_at={}",
                        cp.run_id, cp.current_stage, cp.updated_at
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "multiple incomplete security_engineer checkpoints found; rerun with run_id:\n{list}"
            ))
        }
    }
}
