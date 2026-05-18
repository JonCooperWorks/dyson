# Report Stage

Emit the final structured security harness report. Use only checkpoint facts: findings, rejected candidates, coverage, gaps, dedupe groups, trace evidence, and stage history.

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
  "findings": [],
  "rejected_candidates": [],
  "coverage": [],
  "gaps": [],
  "dedupe_groups": [],
  "trace_evidence": [],
  "stage_history": []
}
```

Do not add findings that are not already in the checkpoint. If there are no confirmed findings, `findings` is an empty array and the useful output is coverage, rejected candidates, gaps, and stage history.
