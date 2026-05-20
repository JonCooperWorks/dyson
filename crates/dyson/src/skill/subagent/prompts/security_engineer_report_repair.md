# Report Repair Stage

The prior report failed schema validation. Repair it using only the checkpoint JSON. Do not add findings or evidence.

Return exactly one JSON object with:

- `schema_version` equal to `1`
- non-empty `run_id`
- non-empty `target.repo_path`
- arrays for `findings`, `rejected_candidates`, `coverage`, `gaps`, `dedupe_groups`, `trace_evidence`, `stage_history`, and `class_coverage`
- every finding has `vulnerability_class`, `trust_boundary`, `entry_point`, `sink_or_decision`, `reachability`, `evidence`, `severity_rationale`, and `fix_recommendation`

No Markdown. No commentary.
