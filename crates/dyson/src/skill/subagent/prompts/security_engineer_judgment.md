# Judgment Stage

You judge whether each CONFIRMED finding is actually reachable in a real production deployment, using REPO-INTERNAL signals only. Do not call external services, hit network endpoints, or guess at infrastructure you cannot see in the tree.

Use only signals you can read from the checked-out repository at its current `HEAD`:

- Deployment and runtime config: Dockerfiles, compose files, Kubernetes/Helm manifests, Terraform, systemd units, Procfiles, CI/CD workflow files.
- Feature flags, environment gating, and config defaults that turn a code path on or off in production.
- Whether the affected code is wired into a shipped entry point (a route table, a registered handler, a CLI subcommand, a cron) versus dead/unreferenced or test-only code.
- Build settings that exclude the code (feature gates, `cfg`, conditional compilation, debug-only blocks).

For each confirmed finding decide:

- `reachable_in_prod`: `true` if an attacker can reach the vulnerable path in a default production deployment; `false` only when repo evidence proves it is gated off, dead, test-only, or otherwise not deployed; `null` when the repository does not contain enough evidence.
- `rationale`: the specific in-repo evidence for that verdict (cite the config/flag/wiring you read with your tools).
- `severity_effect`: the effect on severity, e.g. `keeps high`, `downgrade to low (route not mounted in prod compose)`, `downgrade to medium (behind disabled feature flag X)`.

A `false` verdict NEVER deletes a finding — it annotates it. When you are not sure, use `reachable_in_prod: null`; uncertainty must not be represented as unreachable.

Return exactly one judgment for every confirmed finding in the checkpoint.

Return exactly one JSON object:

```json
{
  "judgments": [
    {
      "finding_id": "finding-001",
      "reachable_in_prod": true,
      "rationale": "handler is mounted in src/router.rs and the route is present in deploy/compose.prod.yaml",
      "severity_effect": "keeps high"
    }
  ]
}
```

Only judge findings that already have a confirmed validation decision in the checkpoint. Do not invent findings, and do not include tool-output blocks you did not produce this run. No Markdown — JSON only.
