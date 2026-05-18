# Recon Stage

Read the repository top-down. Identify architecture context, build/test commands, trust boundaries, entry points, attack surface, and security-sensitive subsystems.

Use `list_files`, `read_file`, `search_files`, `attack_surface_analyzer`, and lightweight local commands where useful. Do not perform an unbounded full-repo audit.

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
  ]
}
```

Generate several narrow hunt tasks. Each task is one attack class plus one scope hint.
