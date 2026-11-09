# dyson deploy

## Overview
This directory holds the container boot wrapper for managed Dyson running under Swarm. `swarm-entrypoint.sh` prepares a credential area for the Codex and Claude CLIs that lives only in tmpfs, then execs the `dyson` binary. Its header comment says provider credentials never enter the sandbox and that durable OAuth state is KMS-sealed by Swarm ([swarm-entrypoint.sh:header]). The image installs the script as `/usr/local/bin/dyson-swarm-entrypoint` and uses it at the end of the `ENTRYPOINT` chain (`dyson/Dockerfile`, `COPY` and `ENTRYPOINT`).

## Interface
- `swarm-entrypoint.sh [args...]`: passes every argument through to `exec /usr/local/bin/dyson "$@"` ([swarm-entrypoint.sh:exec]).
- Exported environment ([swarm-entrypoint.sh:export]):
  - `CODEX_HOME=/dev/shm/dyson-subscriptions/codex`
  - `CLAUDE_CONFIG_DIR=/dev/shm/dyson-subscriptions/claude`

## Invariants
- `set -euo pipefail` ([swarm-entrypoint.sh:set]).
- Refuses to start (exit 1) unless `/dev/shm/dyson-subscriptions` is on a `tmpfs` filesystem, checked with `stat -f -c %T` ([swarm-entrypoint.sh:subscription_root check]).
- The credential directories are created with mode `0700`. `config.toml` is created with mode `0600`, and only if it does not already exist ([swarm-entrypoint.sh:install -d], [swarm-entrypoint.sh:chmod 0600]).

## Side Effects
- Filesystem: creates `/dev/shm/dyson-subscriptions/{codex,claude}`. Seeds `codex/config.toml` with `cli_auth_credentials_store = "file"` if it is absent ([swarm-entrypoint.sh]).
- Process: replaces itself with `/usr/local/bin/dyson` via `exec` ([swarm-entrypoint.sh:exec]).

## Verification
- Run: `bash -n deploy/swarm-entrypoint.sh` (from `dyson/`). The script has no test suite. The workspace tests run with `cargo test`.
