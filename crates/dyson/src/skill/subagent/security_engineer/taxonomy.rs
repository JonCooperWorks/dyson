// ===========================================================================
// Vulnerability taxonomy + class normalization, specialist briefings, and the
// taxonomy-driven hunt task fan-out / coverage tracking.
//
// The taxonomy is a static table of vulnerability classes the harness covers.
// Recon-supplied tasks are canonicalized against it; gaps are filled with a
// generic per-class hunt task so the harness can never silently skip a class.
// ===========================================================================

use std::collections::{BTreeMap, BTreeSet};

use super::report::clean_inline;
use super::types::{
    SecurityCheckpoint, SecurityFinding, SecurityTask, TaskStatus, VulnerabilityClassCoverage,
};

pub(super) const VULNERABILITY_TAXONOMY: &[VulnerabilityClassDefinition] = &[
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
    VulnerabilityClassDefinition {
        id: "race_condition_toctou",
        name: "Race conditions and TOCTOU",
        description: "Time-of-check/time-of-use gaps, non-atomic check-then-act, double-submit/double-spend, concurrent balance/quota/credit updates without locking or atomic operations, and idempotency gaps that only appear under concurrency.",
        examples: &[
            "TOCTOU",
            "check-then-act",
            "double-submit",
            "double-spend",
            "non-atomic balance update",
            "missing row lock",
            "idempotency gap under concurrency",
        ],
        evidence_requirements: &[
            "concurrently reachable entry point",
            "shared mutable state or resource",
            "check and use separated without a lock or atomic step",
            "observable effect of interleaving",
        ],
        detector_keywords: &[
            "lock",
            "mutex",
            "atomic",
            "transaction",
            "for update",
            "balance",
            "quota",
            "credit",
            "idempotency",
            "race",
        ],
    },
    VulnerabilityClassDefinition {
        id: "business_logic_abuse",
        name: "Business logic abuse",
        description: "Abuse of intended workflows that needs no technical exploit: negative or oversized quantities, price/discount/coupon tampering, multi-step flow or state-machine step skipping, replay of one-time actions, and limit/threshold bypass.",
        examples: &[
            "negative quantity",
            "price tampering",
            "coupon/discount abuse",
            "step skipping",
            "state machine bypass",
            "one-time action replay",
            "limit bypass",
        ],
        evidence_requirements: &[
            "business operation entry point",
            "intended workflow or invariant",
            "attacker-controllable parameter or sequence",
            "violated invariant or value effect",
        ],
        detector_keywords: &[
            "price", "amount", "quantity", "discount", "coupon", "balance", "status", "workflow",
            "step", "state", "limit",
        ],
    },
    VulnerabilityClassDefinition {
        id: "mass_assignment_overposting",
        name: "Mass assignment and overposting",
        description: "Binding attacker-controlled request fields directly onto models/records/structs without an allowlist, letting an attacker set privileged fields (role, owner, is_admin, price, tenant) that were never meant to be client-settable.",
        examples: &[
            "mass assignment",
            "overposting",
            "model binding without allowlist",
            "settable role/owner/is_admin",
            "serde flatten of request body into model",
            "update_attributes without strong params",
        ],
        evidence_requirements: &[
            "request body deserialization",
            "target model/record/struct",
            "absence of a field allowlist or guarded attributes",
            "privileged field reachable from input",
        ],
        detector_keywords: &[
            "deserialize",
            "from_json",
            "bind",
            "update_attributes",
            "permit",
            "strong params",
            "flatten",
            "model",
            "role",
            "owner",
        ],
    },
    VulnerabilityClassDefinition {
        id: "denial_of_wallet_cost_abuse",
        name: "Denial-of-wallet and cost abuse",
        description: "Attacker-triggered unbounded spend on paid downstreams — LLM/model tokens, cloud APIs, egress, storage — via missing per-user/per-instance quotas, unmetered loops, amplification, or retries without caps.",
        examples: &[
            "unbounded LLM/token spend",
            "unmetered paid API calls",
            "missing per-user quota",
            "retry without cap",
            "amplification",
            "egress cost abuse",
        ],
        evidence_requirements: &[
            "attacker-reachable trigger",
            "paid downstream call",
            "per-actor quota/budget/rate enforcement point",
            "amplification or loop factor",
        ],
        detector_keywords: &[
            "openrouter",
            "llm",
            "token",
            "quota",
            "budget",
            "rate",
            "limit",
            "retry",
            "spend",
            "cost",
            "billing",
            "egress",
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

/// Tree-sitter `ast_query` patterns tuned for a single vulnerability class.
///
/// Hunters are told to "prefer AST and taint evidence over grep" but were
/// never shown the query shapes for their class — a weaker model writes one
/// bad S-expression, eats the error, and silently falls back to grep.  These
/// give each class specialist a concrete starting point to run and narrow.
/// Patterns are intentionally broad (capture the sink family, then inspect),
/// and span the common languages so the specialist can pick the one matching
/// the target.  Classes without high AST leverage return an empty slice; the
/// briefing still carries their evidence requirements and detector seeds.
pub(super) fn class_ast_hints(class_id: &str) -> &'static [&'static str] {
    match class_id {
        "auth_authorization" => &[
            "Locate authorization checks, then find handlers that never call one. Rust: `(call_expression function: (field_expression field: (field_identifier) @m) (#match? @m \"authorize|is_admin|owner|tenant|require_|verify_\"))`.",
            "A handler that takes an attacker-controlled id and reaches a store lookup with no preceding owner/tenant predicate is the candidate — confirm with `taint_trace` from the id to the lookup.",
        ],
        "session_oauth_csrf" => &[
            "Redirect sinks: `(call_expression function: (_) @f (#match? @f \"redirect|Redirect|Location|set_location\"))` — check the target is not attacker-controlled.",
            "Find `state`/`nonce`/`pkce` handling and flag missing checks or non-constant-time `==` comparisons on them.",
        ],
        "ssrf_outbound_network" => &[
            "Outbound clients taking a URL var. Rust: `(call_expression function: (field_expression field: (field_identifier) @m (#match? @m \"get|post|request|send\")))`; then `taint_trace` the URL back to request input.",
            "Look for redirect-follow defaults and the absence of private/link-local/metadata (169.254.x) address rejection before the request fires.",
        ],
        "proxy_http_boundary" => &[
            "Header forwarding: find where inbound headers are copied to an upstream request; flag hop-by-hop or `authorization`/`x-forwarded-*` passed across a trust boundary.",
        ],
        "container_sandbox_runtime" => &[
            "Process/container launch sinks. Rust: `(call_expression function: (scoped_identifier) @f (#match? @f \"Command::new|process::Command\"))`; inspect args/env/mounts for injection or host exposure.",
        ],
        "secrets_credentials" => &[
            "Secrets reaching logs: `(macro_invocation macro: (identifier) @m (#match? @m \"info|warn|error|debug|trace|println|eprintln\"))`, then inspect args for token/secret/key/password identifiers.",
        ],
        "file_archive_path" => &[
            "File sinks taking a path var: open/read/write/`File::`/`fs::`; `taint_trace` the path to request input and check for canonicalization / `..` rejection in between.",
            "Archive extraction loops that write entry names without validating the destination stays under the target dir.",
        ],
        "injection_unsafe_execution" => &[
            "Command exec — Python: `(call function: (attribute attribute: (identifier) @m (#match? @m \"system|popen|exec\")))`; JS/TS: `(call_expression function: (member_expression property: (property_identifier) @m (#match? @m \"exec|execSync|spawn\")))`; Rust: `Command::new`.",
            "SQL/template/eval: find query/render/eval sinks fed by string concatenation of attacker input rather than parameterization.",
        ],
        "crypto_randomness" => &[
            "Non-CSPRNG for security values: `(call_expression function: (_) @f (#match? @f \"thread_rng|rand::random|Math.random|random\\\\.\"))` feeding tokens/ids/nonces.",
            "Secret/MAC comparison with `==` instead of a constant-time compare.",
        ],
        "multi_tenant_isolation" => &[
            "Store lookups by id missing an owner_id/tenant_id/instance_id predicate — ast_query the query-builder calls and inspect their filters; list endpoints that select without a tenant scope.",
        ],
        "webhooks_inbound_integrations" => &[
            "Signature verification calls (hmac/verify/`==` on a signature); flag handlers that read/act on the body before or without verifying, and missing timestamp/replay windows.",
        ],
        "resource_exhaustion_dos" => &[
            "Unbounded reads: `(call_expression function: (field_expression field: (field_identifier) @m (#match? @m \"read_to_end|read_to_string|bytes|body|collect\")))` with no size/element cap on an attacker-triggered path.",
        ],
        "agent_tool_boundary" => &[
            "Find where tool/MCP allowlists are checked, then paths that dispatch a tool without the check; untrusted content (web/file/tool output) that can steer a privileged tool call.",
        ],
        "api_contract_input_validation" => &[
            "Deserialization with permissive defaults / enum fallthrough: `(call_expression function: (_) @f (#match? @f \"from_str|from_slice|deserialize|parse\"))`; check default branches that permit on invalid input.",
        ],
        "race_condition_toctou" => &[
            "Find check-then-act: a read/exists/get followed by a write/update on the same resource with no lock or transaction between them.",
            "Balance/quota/credit mutations that read-modify-write without an atomic op or `SELECT ... FOR UPDATE` — `ast_query` the update call, then inspect the surrounding read.",
        ],
        "business_logic_abuse" => &[
            "Handlers that read numeric amount/quantity/price/status fields from the request and use them without range or invariant validation.",
            "Multi-step flows where a later step does not verify the prior step completed — locate the step handler and check it re-validates state.",
        ],
        "mass_assignment_overposting" => &[
            "Request-body deserialization into a model/struct: Rust `(call_expression function: (_) @f (#match? @f \"from_str|from_slice|deserialize\"))`; then inspect the target type for privileged fields with no allowlist.",
            "`serde(flatten)` or wholesale `update(body)` / `update_attributes(params)` that copy every request field onto a record.",
        ],
        "denial_of_wallet_cost_abuse" => &[
            "Paid-downstream calls (LLM/model/provider/cloud client) on an attacker-reachable path — `ast_query` the client call, then check for a per-actor quota/budget gate before it.",
            "Loops or retries that issue paid calls without a cap or bounded backoff.",
        ],
        _ => &[],
    }
}

/// Build the class-specialist briefing appended to the Hunt stage prompt so
/// the spawned child is a dedicated hunter for exactly one vulnerability
/// class — briefed with that class's evidence requirements, detector seeds,
/// and the tree-sitter `ast_query` patterns to run instead of falling back to
/// grep.  Returns `None` for follow-up classes not in the taxonomy (e.g.
/// `consumer_path_review`), which fall back to the generic hunt prompt.
pub(super) fn class_specialist_briefing(class_id: &str) -> Option<String> {
    let class = VULNERABILITY_TAXONOMY.iter().find(|c| c.id == class_id)?;
    let mut b = String::new();
    b.push_str("## Your specialization\n\n");
    b.push_str(&format!(
        "You are the dedicated **{}** (`{}`) hunter for this run. Hunt this class only; \
         do not chase other classes — separate specialists cover them.\n\n",
        class.name, class.id
    ));
    b.push_str(&format!("Focus: {}\n\n", class.description));
    b.push_str("Evidence you must collect before reporting a candidate:\n");
    for req in class.evidence_requirements {
        b.push_str(&format!("- {req}\n"));
    }
    b.push_str("\nSearch seeds (detector keywords — starting points, not an allowlist): ");
    b.push_str(&format!("`{}`.\n\n", class.detector_keywords.join("`, `")));
    let hints = class_ast_hints(class.id);
    if !hints.is_empty() {
        b.push_str(
            "AST query patterns for this class. Run these with `ast_query` (pass `language` \
             or an `include` glob), then narrow from the matches — prefer this over grep:\n",
        );
        for hint in hints {
            b.push_str(&format!("- {hint}\n"));
        }
        b.push('\n');
    }
    b.push_str(
        "Workflow: enumerate entry points with `attack_surface_analyzer`, locate the \
         sink/decision with the AST patterns above, then prove the connection with \
         `taint_trace`. Report a candidate only with a source, a sink/decision, and a \
         reachability claim backed by a real tool call.\n",
    );
    Some(b)
}

pub(super) fn canonical_vulnerability_class(value: &str) -> Option<&'static str> {
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

pub(super) fn normalize_class_id(value: &str) -> String {
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

pub(super) fn taxonomy_class(class_id: &str) -> Option<&'static VulnerabilityClassDefinition> {
    canonical_vulnerability_class(class_id)
        .and_then(|id| VULNERABILITY_TAXONOMY.iter().find(|class| class.id == id))
}

pub(super) fn build_class_coverage(
    _scope: &str,
    _architecture_context: &str,
    provided: Vec<VulnerabilityClassCoverage>,
) -> Vec<VulnerabilityClassCoverage> {
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
            let mut item = provided_by_id.remove(class.id).unwrap_or_default();
            item.class_id = class.id.into();
            item.class_name = class.name.into();
            item.considered = true;
            // Every class is hunted unconditionally (see
            // ensure_taxonomy_hunt_tasks). Clear any stale skipped_reason
            // so the report doesn't claim a class was skipped when in fact
            // a specialist is going to look at it.
            item.applicable = true;
            item.skipped_reason.clear();
            item
        })
        .collect()
}

pub(super) fn canonicalize_tasks(tasks: &mut [SecurityTask]) {
    for task in tasks {
        if let Some(class_id) = canonical_vulnerability_class(&task.attack_class) {
            task.attack_class = class_id.into();
        }
    }
}

pub(super) fn canonicalize_findings(findings: &mut [SecurityFinding]) {
    for finding in findings {
        if let Some(class_id) = canonical_vulnerability_class(&finding.vulnerability_class) {
            finding.vulnerability_class = class_id.into();
        }
    }
}

// Queue a hunt task for EVERY class in the taxonomy, regardless of what
// the recon stage said. Rationale:
//
// 1. Weaker / non-Claude models drop the `class_coverage` field, mark
//    everything as `applicable=false`, or hallucinate reasons to skip.
//    The old "only queue applicable" gating let the orchestrator silently
//    skip entire vulnerability classes that the user asked for.
// 2. Per-class hunt specialists are cheap when there's nothing to find:
//    they grep the scope, emit empty findings, and exit. Letting the
//    specialist decide "no work here" is more reliable than letting the
//    recon model decide "no need to spawn this specialist."
// 3. The user explicitly asked for "just run every agent on it."
//
// Recon-supplied tasks still take precedence for their class — they
// carry the model's narrower scope_hint and rationale. Only classes the
// recon model did NOT cover get the generic taxonomy-driven task.
pub(super) fn ensure_taxonomy_hunt_tasks(
    checkpoint: &SecurityCheckpoint,
    tasks: &mut Vec<SecurityTask>,
) {
    let mut covered_classes: BTreeSet<&str> = tasks
        .iter()
        .filter_map(|task| canonical_vulnerability_class(&task.attack_class))
        .collect();
    let mut next = tasks.len() + 1;
    for class in VULNERABILITY_TAXONOMY {
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

pub(super) fn class_scope_hint(
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

pub(super) fn update_class_coverage_task_ids(
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

pub(super) fn mark_hunted_class_coverage(
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
