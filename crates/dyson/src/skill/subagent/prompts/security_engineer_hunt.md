# Hunt Stage

Work only on the pending tasks shown in the checkpoint JSON. Each task is one attack class plus one scope hint.

You may use AST tools, `taint_trace`, read/search/list, dependency review, and local scratch PoC/test commands for owned local code. Prefer AST and taint evidence over grep. Do not ask one agent to find every vulnerability in the repo.

Only return evidence-backed candidates. If evidence is incomplete, report a gap or follow-up task instead of inflating a finding.

Return exactly one JSON object:

```json
{
  "completed_task_ids": ["hunt-001"],
  "findings": [
    {
      "id": "finding-001",
      "title": "short title",
      "severity": "critical|high|medium|low|informational",
      "root_cause": "shared root cause",
      "affected_paths": ["path/file.rs:123"],
      "evidence": ["tool-backed or code-backed evidence"],
      "reachability": "known reachable|suspected|not traced"
    }
  ],
  "gaps": [
    {
      "area": "uncovered area",
      "reason": "why not covered",
      "risk": "low|medium|high|critical"
    }
  ],
  "follow_up_tasks": [
    {
      "id": "gap-001",
      "attack_class": "consumer_path_review",
      "scope_hint": "specific follow-up scope",
      "rationale": "why this follow-up is needed"
    }
  ]
}
```
