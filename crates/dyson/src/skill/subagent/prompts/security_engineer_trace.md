# Trace Stage

Trace confirmed shared-library or reusable-component findings from real entry points to the flaw.

Use `taint_trace` where appropriate. Treat taint output as lossy: verify each hop with `read_file`. Reachability must affect severity.

Return exactly one JSON object:

```json
{
  "traces": [
    {
      "finding_id": "finding-001",
      "reachable": true,
      "severity_effect": "raises|keeps|downgrades",
      "evidence": ["entry point to sink evidence, including real taint_trace output only if actually run"]
    }
  ]
}
```
