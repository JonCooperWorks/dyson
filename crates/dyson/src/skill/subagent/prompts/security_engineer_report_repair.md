# Report Repair Stage

The prior report failed schema validation. Repair it using only the checkpoint JSON. Do not add findings or evidence.

Return exactly one JSON object with:

- `schema_version` equal to `1`
- non-empty `run_id`
- non-empty `target.repo_path`
- arrays for `findings`, `rejected_candidates`, `coverage`, `gaps`, `dedupe_groups`, `trace_evidence`, and `stage_history`

No Markdown. No commentary.
