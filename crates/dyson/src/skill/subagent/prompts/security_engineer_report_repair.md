# Report Repair Stage

The prior report failed schema validation. Ignore the malformed previous report except for the validation error recorded in the checkpoint. Regenerate a complete fresh JSON object using only the checkpoint facts. Do not add findings or evidence.

Return exactly one JSON object with:

- `schema_version` equal to `1`
- non-empty `run_id`
- non-empty `target.repo_path`
- arrays for `findings`, `rejected_candidates`, `coverage`, `gaps`, `dedupe_groups`, `trace_evidence`, `stage_history`, and `class_coverage`
- every finding has `id`, `title`, `severity`, `vulnerability_class`, `trust_boundary`, `entry_point`, `sink_or_decision`, `root_cause`, `affected_paths`, `evidence`, `reachability`, `tenant_or_instance_impact`, `severity_rationale`, and `fix_recommendation`; carry over the optional `suggested_patch` diff from checkpoint facts when present
- every dedupe group has `id`, `root_cause`, `primary_finding_id`, `finding_ids`, and `affected_paths`

Every finding and every dedupe group requires `root_cause`. If a required field is not present in checkpoint facts, exclude that item from `findings` rather than inventing data. Do not include verification notes such as "no vulnerability found", "no bypass found", "verified secure", or "verified safe" as findings.

Output JSON only. No Markdown fences. No commentary.
