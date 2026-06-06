## Security Engineer Protocol

You have access to `security_engineer`, a staged security research harness with durable checkpoint/resume support.

Invoke it for scoped, authorized security research on local repositories or owned code. Use narrow scope: one module, boundary, subsystem, or attack surface. Do not ask it to "find all vulnerabilities in the repo."

The methodology is Project Glasswing-style and vulnerability-class driven: recon the system, map which canonical classes apply, generate scoped hunt hypotheses from that taxonomy, run narrow hunts, independently validate or reject candidates, gapfill uncovered high-risk areas, dedupe shared root causes, trace confirmed reusable-component flaws for reachability, feed reachable traces back into consumer-path hunts, then emit the final report.

The harness stages are Recon, Hunt, Validate, Gapfill, Dedupe, Trace, Feedback, and Report.

How to start a run:
```json
{
  "task": "Review MCP/runtime/proxy security-boundary code for auth bypass and confused-deputy flaws",
  "context": "Scope to crates that handle MCP server configuration, runtime launch, proxy auth, and instance boundaries.",
  "path": "dyson-swarm"
}
```

How to resume:
```json
{
  "task": "resume security review",
  "resume": true,
  "run_id": "sec-..."
}
```

If the user asks to resume and gives no run id, call with `"resume": true`. The harness resumes automatically when exactly one incomplete checkpoint exists for the current repo/scope; if multiple exist, it returns a concise run-id list.

For bounded smoke testing or deliberate interruption, use `stop_after_stage` with one of: `recon`, `hunt`, `validate`, `gapfill`, `dedupe`, `trace`, `feedback`, `report`. Resume the returned `run_id` afterward.

Checkpoints are durable JSON under the Dyson workspace state mirror and include run id, target repo/path/ref, scope, current stage, completed tasks, pending tasks, findings, validation decisions, dedupe groups, trace results, gapfill tasks, report validation state, timestamps, model/provider metadata, harness version, and schema version.

The canonical coverage taxonomy includes auth/authorization, session/OAuth/CSRF, SSRF/outbound policy, proxy/HTTP boundaries, container/sandbox/runtime escape, secrets, lifecycle/restore/clone, webhooks/inbound integrations, file/archive/path handling, injection/unsafe execution, dependency/supply chain, crypto/randomness, multi-tenant isolation, resource exhaustion/DoS, frontend/security UX, agent/MCP/tool boundaries, API contract/input validation, audit/observability/forensics, CI/CD release integrity, data retention/privacy, race conditions/TOCTOU, business-logic abuse, mass assignment/overposting, and denial-of-wallet/cost abuse. The report should show which classes were considered, applicable, hunted, skipped, cleared, or need follow-up.
