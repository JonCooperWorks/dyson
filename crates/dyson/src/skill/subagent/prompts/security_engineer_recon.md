# Recon Stage

Read the repository top-down. Identify architecture context, app type, languages/frameworks, build/test commands, trust boundaries, entry points, attack surface, and security-sensitive subsystems.

Use `list_files`, `read_file`, `search_files`, `attack_surface_analyzer`, and lightweight local commands where useful. Do not perform an unbounded full-repo audit.

Consider every canonical vulnerability class from the base prompt. Mark each class applicable when detected architecture/components make it relevant; mark skipped only with a concrete reason. Generate hunt tasks from applicable classes, not generic "review security" tasks.

Return exactly one JSON object:

```json
{
  "architecture_context": "short evidence-backed architecture summary",
  "tasks": [
    {
      "id": "hunt-001",
      "attack_class": "auth_bypass",
      "scope_hint": "src/http/auth.rs and adjacent middleware",
      "rationale": "why this narrow hunt matters"
    }
  ],
  "coverage_gaps": [
    {
      "area": "subsystem not covered",
      "reason": "why not covered yet",
      "risk": "low|medium|high|critical"
    }
  ],
  "class_coverage": [
    {
      "class_id": "auth_authorization",
      "class_name": "Authentication and authorization",
      "considered": true,
      "applicable": true,
      "hunted": false,
      "skipped_reason": "",
      "high_risk_follow_up": false,
      "checked_and_cleared": false,
      "task_ids": ["hunt-001"],
      "evidence": ["route/middleware files indicate auth boundary"]
    }
  ]
}
```

Generate several narrow hunt tasks. Each task is one canonical vulnerability class plus one scope hint.
