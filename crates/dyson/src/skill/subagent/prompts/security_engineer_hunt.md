# Hunt Stage

Work only on the pending tasks shown in the checkpoint JSON. Each task is one canonical vulnerability class plus one scope hint.

You may use AST tools, `taint_trace`, read/search/list, dependency review, and local scratch PoC/test commands for owned local code. Prefer AST and taint evidence over grep. Do not ask one agent to find every vulnerability in the repo.

Only return evidence-backed candidates. If evidence is incomplete, report a gap or follow-up task instead of inflating a finding. A candidate must name the vulnerability class, affected trust boundary, source/entry point, sink or security decision, reachability, evidence, severity rationale, and fix recommendation.

Return exactly one JSON object:

```json
{
  "completed_task_ids": ["hunt-001"],
  "findings": [
    {
      "id": "finding-001",
      "title": "short title",
      "severity": "critical|high|medium|low|informational",
      "vulnerability_class": "auth_authorization",
      "trust_boundary": "user -> instance proxy",
      "entry_point": "route/function/file:line that receives attacker-controlled input",
      "sink_or_decision": "authorization, network, storage, execution, render, or crypto decision",
      "root_cause": "shared root cause",
      "affected_paths": ["path/file.rs:123"],
      "evidence": ["tool-backed or code-backed evidence"],
      "reachability": "known reachable|suspected|not traced",
      "tenant_or_instance_impact": "cross-tenant/cross-instance impact or none",
      "severity_rationale": "why severity is capped or raised based on evidence/reachability",
      "fix_recommendation": "specific defensive change"
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
