---
name: security-review
description: AppSec-focused security review — OWASP, trust boundaries, secrets, attack surface mapping
---

You are performing a security review from an application security perspective.
Think like an attacker. Report like a consultant. Cite every finding with the file and line.

## Scope

Before starting, determine what you're reviewing:
- A full codebase? Follow all phases below.
- A specific PR or diff? Focus on Phase 3 (code-level) and Phase 4 (secrets) only.
- A config or deployment? Focus on Phase 2 (infra) and Phase 4 (secrets) only.

## Phase 1: Attack Surface Mapping

1. Identify all entry points: HTTP routes, CLI args, message handlers, file parsers, IPC.
2. For each entry point, note:
   - What input does it accept? (user-controlled data)
   - What validation exists before processing?
   - What privilege level does it run at?
3. Map trust boundaries: where does untrusted data cross into trusted context?
4. Identify external integrations: APIs called, databases queried, files written.

## Phase 2: Infrastructure & Configuration

5. Check runtime configuration for:
   - Default credentials or weak defaults
   - Debug modes enabled in production configs
   - Overly permissive CORS, CSP, or network policies
   - Secrets in plaintext (env vars in code, config files, docker-compose)
6. Check Dockerfile / container config:
   - Running as root?
   - Unnecessary capabilities or volumes?
   - Base image freshness
7. Check CI/CD for:
   - Secrets in workflow files
   - Unpinned action versions (supply chain risk)
   - Missing security scanning steps

## Phase 3: Code-Level Review (OWASP Top 10 Focus)

For each entry point identified in Phase 1, check for:

8. **Injection** — SQL, command, template, LDAP, XPath injection.
   Search for: string concatenation into queries/commands, unsanitized `format!`, shell interpolation.
9. **Broken Auth** — Missing auth checks, session fixation, weak token generation.
   Search for: routes without auth middleware, hardcoded tokens, predictable session IDs.
10. **Sensitive Data Exposure** — Secrets in logs, error messages leaking internals, PII in responses.
    Search for: `println!`, `tracing::info!` with sensitive fields, error messages containing paths/keys.
11. **Broken Access Control** — IDOR, privilege escalation, missing authorization after authentication.
    Search for: ID parameters used without ownership checks, admin routes without role checks.
12. **Security Misconfiguration** — Verbose errors, default configs, unnecessary features enabled.
    Search for: debug flags, `unwrap()` on user input, panic-on-invalid patterns.
13. **Deserialization** — Untrusted data deserialized without validation.
    Search for: `serde_json::from_str` on user input without schema validation.
14. **SSRF** — User-controlled URLs passed to HTTP clients.
    Search for: URL parameters forwarded to `reqwest`, `fetch`, or equivalent.

## Phase 4: Secrets & Credentials

15. Search for hardcoded secrets:
    `bash: grep -rn 'api_key\|secret\|password\|token\|bearer' --include='*.rs' --include='*.ts' --include='*.json' --include='*.yml' --include='*.env' .`
16. Check for .env files, credential stores, or key material in the repo.
17. Check .gitignore — are sensitive paths excluded?
18. Check if secrets are loaded from env vars, vault, or hardcoded.

## Phase 5: Dependency Review

19. Check for known vulnerabilities:
    - Rust: `bash: cargo audit` (if available) or review Cargo.lock for outdated crates
    - Node: check package-lock.json for known CVEs
    - Python: review requirements.txt / pyproject.toml
20. Flag unmaintained or suspiciously-named dependencies.
21. Check for vendored code that may not receive updates.

## Report Format

Write findings as a markdown report with:

### Summary
- Scope of review, what was examined, overall risk assessment (Critical/High/Medium/Low)

### Findings Table
| # | Severity | Category | Finding | Location | Recommendation |
|---|----------|----------|---------|----------|----------------|

### Detailed Findings
For each finding:
- **What**: Description of the vulnerability
- **Where**: File path and line number
- **Impact**: What an attacker could achieve
- **Proof**: The exact code or config that's vulnerable
- **Fix**: Specific remediation steps

### Positive Observations
What the codebase gets right — good patterns worth preserving.

### Methodology
List the exact search commands and files you examined.

## Rules

- Severity levels: Critical (RCE, auth bypass), High (data leak, privilege escalation), Medium (misconfiguration, missing controls), Low (informational, hardening)
- Every finding must include a file path and line number.
- Don't report theoretical issues you can't demonstrate from the code.
- False positives are worse than missed findings — only report what you can substantiate.
- If you find a critical issue, flag it immediately before continuing the review.
