# Managed Dyson HTTP authentication

Cube snapshots preserve the process started during template warmup. Environment
variables injected on restore cannot replace that process's HTTP authenticator.
Managed templates therefore boot with `auth.type: swarm` and an operator bootstrap
bearer hash, never `dangerous_no_auth`.

The deploy runbook provisions `dyson.bootstrap_token` in Swarm's sealed system
store. Only its Argon2 hash is copied into `/etc/dyson/bootstrap-auth.hash` in the
image. Keep `.build/dyson-bootstrap-auth.hash` with deployment backups. The helper
refuses to silently replace either half of an existing trust pair. Bootstrap
rotation must account for every still-available template generation.

On configure, Swarm sends the instance bearer in `http_bearer`. It first
authenticates with that bearer (cold boots), then retries a 401 with the bootstrap
bearer (snapshot warmup). No anonymous retry exists. Before a configure secret
has been pinned, Dyson requires the current HTTP authenticator or a matching
trusted configure preseed. Once pinned, only the same configure secret can change
configuration; bootstrap cannot reset it.

Dyson hashes the instance bearer, persists the hash in the HTTP controller config,
installs it in memory before returning success, and invalidates outstanding SSE
tickets. Ordinary APIs, SSE ticket issuance, and Telegram webhook ingestion all
use the current authenticator. Auth discovery reports `mode: bearer`; liveness
and static assets remain public. Swarm rejects configure acknowledgements without
`http_auth_applied: true`, so an older template cannot silently retain anonymous
access after a successful provisioning response.

Only the existing configure-protected admin lifecycle routes can authenticate
with their pinned configure secret independently of the instance bearer.
`cost-backfill` and other API paths retain the normal bearer gate. Explicit local
`dangerous_no_auth` deployments retain their existing opt-in behavior.

Verification: the original warmup regression returned 200 instead of 401 before
the fix. Socket-level tests cover anonymous and forged requests, normalized paths,
bootstrap enrollment, instance bearer access, lifecycle admin access, persistence,
SSE issuance, and Telegram ingestion. Provisioning tests ensure stdin-only secret
transport and stable trust across repeated deployments. No LLM inference is needed.
