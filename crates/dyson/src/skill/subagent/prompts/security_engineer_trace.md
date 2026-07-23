# Trace Stage

Trace confirmed shared-library or reusable-component findings from real entry points to the flaw.

Use `taint_trace` where appropriate. Treat taint output as lossy: verify each hop with `read_file`. Reachability must affect severity.

Return exactly one trace for every confirmed finding in the checkpoint. If reachability cannot be established, use `"reachable": null`, `"severity_effect": "unknown"`, and explain what evidence is missing. Never use `false` merely because tracing was incomplete.

Return exactly one JSON object:

```json
{
  "traces": [
    {
      "finding_id": "finding-001",
      "reachable": true,
      "severity_effect": "raises|keeps|downgrades",
      "evidence": ["entry point to sink evidence, including real taint_trace output only if actually run"],
      "consumer_paths": ["path/to/downstream/caller.rs:42"]
    }
  ]
}
```

`consumer_paths` contains only concrete downstream callers or shipped entry points discovered during tracing. Do not repeat the vulnerable sink path merely to populate the field. Use an empty array when no additional consumer was found.
