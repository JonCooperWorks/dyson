# Report Stage

Emit the final structured security harness report. Use only checkpoint facts: findings, rejected candidates, vulnerability-class coverage, gaps, dedupe groups, trace evidence, and stage history.

Return exactly one JSON object matching this schema:

```json
{
  "schema_version": 1,
  "run_id": "sec-...",
  "target": {
    "repo_path": "/path/to/repo",
    "git_ref": "optional git sha"
  },
  "scope": "scope text",
  "findings": [
    {
      "id": "finding-001",
      "title": "short title",
      "severity": "high",
      "vulnerability_class": "auth_authorization",
      "trust_boundary": "caller-to-service boundary",
      "entry_point": "route or caller",
      "sink_or_decision": "authorization decision or sink",
      "root_cause": "specific missing or incorrect security decision",
      "affected_paths": ["src/path.rs:10"],
      "evidence": ["tool-backed evidence"],
      "reachability": "reachable or not traced",
      "tenant_or_instance_impact": "tenant or instance impact",
      "severity_rationale": "why the severity follows from evidence",
      "fix_recommendation": "specific fix",
      "suggested_patch": "optional minimal unified diff that applies the fix"
    }
  ],
  "rejected_candidates": [],
  "coverage": [],
  "gaps": [],
  "dedupe_groups": [
    {
      "id": "dedupe-001",
      "root_cause": "shared root cause",
      "primary_finding_id": "finding-001",
      "finding_ids": ["finding-001"],
      "affected_paths": ["src/path.rs:10"]
    }
  ],
  "trace_evidence": [],
  "stage_history": [],
  "class_coverage": []
}
```

Every finding must include `id`, `title`, `severity`, `vulnerability_class`, `trust_boundary`, `entry_point`, `sink_or_decision`, `root_cause`, `affected_paths`, `evidence`, `reachability`, `tenant_or_instance_impact`, `severity_rationale`, and `fix_recommendation`.

`suggested_patch` is optional but encouraged: when the fix is small and local, emit a minimal unified diff (standard `--- a/path` / `+++ b/path` / `@@` hunk format) that a human can review and apply. It is a suggestion only — it is never applied automatically. Omit it (or use an empty string) when the fix is too broad to express as a small diff. Never fabricate file contents you have not read.

Every dedupe group must include `id`, `root_cause`, `primary_finding_id`, `finding_ids`, and `affected_paths`.

Do not add findings that are not already in the checkpoint. Do not include verification notes such as "no vulnerability found", "no bypass found", "verified secure", or "verified safe" as findings. If there are no confirmed reportable findings, `findings` is an empty array and the useful output is class coverage, checked-and-cleared classes, rejected candidates, gaps, and stage history.
