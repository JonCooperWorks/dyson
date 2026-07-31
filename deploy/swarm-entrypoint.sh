#!/usr/bin/env bash
# Managed-Dyson boot wrapper. ChatGPT subscription credentials must survive
# individual Codex subprocesses, but must never land in the cube's writable
# layer or the swarm state mirror. A private tmpfs gives exactly that lifetime:
# the current microVM (including pause/resume), and no longer.
set -euo pipefail

subscription_root=/dev/shm/dyson-subscriptions
codex_root="${subscription_root}/codex"

install -d -m 0700 "${subscription_root}"
if [[ "$(stat -f -c %T "${subscription_root}")" != "tmpfs" ]]; then
  echo "refusing to start: subscription credential store is not tmpfs" >&2
  exit 1
fi

install -d -m 0700 "${codex_root}"
if [[ ! -f "${codex_root}/config.toml" ]]; then
  printf '%s\n' 'cli_auth_credentials_store = "file"' > "${codex_root}/config.toml"
  chmod 0600 "${codex_root}/config.toml"
fi

export CODEX_HOME="${codex_root}"
exec /usr/local/bin/dyson "$@"
